# API & Remote Mode

awman can be driven over HTTP instead of from an interactive terminal. This guide covers both halves of that:

- **[API mode](#api-mode)** — the *server*. `awman api start` exposes awman's sessions and subcommands over HTTP so scripts, CI pipelines, and any HTTP client can drive them. This half also covers [non-interactive (headless) operation](#non-interactive-headless-operation), which applies to plain CLI use too.
- **[Remote mode](#remote-mode)** — the *client*. `awman remote …` is a thin command-line client for an awman API server, with live log streaming, so you can dispatch work to a build host as if it were local.

You do not need remote mode to use the API (curl works fine), and you do not need to understand the HTTP surface to use `awman remote`. Read whichever half you need.

---

# API mode

API mode exposes awman's session and subcommand execution over HTTP. Start a persistent server with `awman api start`, then drive sessions and subcommands from scripts, CI pipelines, or any HTTP client — no interactive terminal or TUI required.

A **session** in API mode is conceptually identical to a TUI tab: an isolated workspace bound to a working directory. Commands dispatched to a session (`exec prompt`, `exec workflow`, and the others listed under [Submit a command](#submit-a-command)) execute exactly as they would in a TUI tab — inside a container on the configured runtime, with the same security and isolation guarantees.

API metadata, credentials, and output logs are recorded durably under
`~/.awman/api/` for auditability. The shared SQLite database is stored at
`~/.awman/data/awman.db`.

API mode is for request-driven sessions and commands. To hand recurring work to
a group of agents that pick it up on their own, build a [squad](12-squad.md)
instead. The API server and the squad daemon behind your squad share the
database and are mutually exclusive on one machine.

---

## When to use API mode

API mode is useful for:

- CI pipelines that trigger `exec workflow` or `exec prompt` runs and poll for results
- Scripts or tooling that execute workflows and retrieve output programmatically
- Remote integrations where the awman server runs on a build host and clients reach it through a tunnel or proxy
- Audit-heavy environments where a complete durable record of every agent action is required

For single interactive sessions, use `awman chat` instead.

---

## Quickstart — copy-pastable examples

These examples assume a server on `127.0.0.1:9876` (the server always listens on loopback; see [Reaching the server from another machine](#reaching-the-server-from-another-machine)). It speaks HTTPS with a self-signed cert by default, so curl needs `-k` (or `--cacert ~/.awman/api/tls/cert.pem`). Pass `--dangerously-skip-tls` at server start if you'd rather use plain HTTP.

```sh
# 0. Start the server. The first start generates an API key and prints it
#    ONCE in a banner; copy it now.
awman api start --port 9876 --workdirs "$HOME/my-project" --background

# Stash the key the banner just printed:
KEY=<paste-key-from-banner>
SERVER=https://127.0.0.1:9876
```

### Local session

A local session binds to a directory that's already on the server's allowlist (configured via `--workdirs` or `api.workDirs`).

**Using `curl`:**

```sh
# 1. Create a session bound to a workdir
SESSION=$(curl -sk -X POST "$SERVER/v1/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"session_type\":\"local\",\"workdir\":\"$HOME/my-project\"}" | jq -r .session_id)

# 2. Wait for session setup (it runs `ready`) to finish
until [ "$(curl -sk "$SERVER/v1/sessions/$SESSION/status" \
  -H "Authorization: Bearer $KEY" | jq -r .status)" = ready ]; do sleep 2; done

# 3. Dispatch a one-shot prompt; the response carries the command_id
CMD=$(curl -sk -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec prompt","args":["Summarise recent changes","--non-interactive"]}' \
  | jq -r .command_id)

# 4. Stream the output until the command finishes
curl -Nsk "$SERVER/v1/commands/$CMD/logs" \
  -H "Authorization: Bearer $KEY" \
  | while IFS= read -r line; do
      case "$line" in
        "event: done") break ;;
        data:\ *)      printf '%s\n' "${line#data: }" ;;
      esac
    done

# 5. Close the session
curl -sk -X DELETE "$SERVER/v1/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY"
```

[`POST /v1/runs`](#one-shot-runs) combines steps 1–3 (and optionally 5) into one request.

**Using `awman` itself** (`awman remote` waits for readiness for you):

```sh
# One-time setup so subsequent commands don't need flags
awman config set --global remote.defaultAddr "$SERVER"
awman config set --global remote.defaultAPIKey "$KEY"

# Start a session (returns once setup is ready) and capture its ID
SESSION=$(awman remote session start --workdir "$HOME/my-project" \
  | awk '/ created via / {print $2}')

# Dispatch the same prompt and stream its output live
awman remote exec prompt "Summarise recent changes" \
  --session "$SESSION" --follow

# Close the session
awman remote session kill "$SESSION"
```

### Remote session (server clones a Git repo)

A remote session asks the server to clone a Git repo under `<API root>/sessions/<session-id>/<repository-folder>`. The clone is deleted when the session is closed.

**Using `curl`:**

```sh
# 1. Create a remote session — the server does the clone for you
SESSION=$(curl -sk -X POST "$SERVER/v1/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "session_type": "remote",
    "repo_url": "https://github.com/org/my-project",
    "branch": "main"
  }' | jq -r .session_id)

# 2. Wait for the clone and setup to finish ("failed" means setup failed)
until [ "$(curl -sk "$SERVER/v1/sessions/$SESSION/status" \
  -H "Authorization: Bearer $KEY" | jq -r .status)" = ready ]; do sleep 5; done

# 3. Run a workflow against the clone
CMD=$(curl -sk -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec workflow","args":["aspec/workflows/implement-feature.toml","--work-item","0053"]}' \
  | jq -r .command_id)

# 4. Poll status until terminal
while true; do
  STATUS=$(curl -sk "$SERVER/v1/commands/$CMD" \
    -H "Authorization: Bearer $KEY" | jq -r .status)
  echo "status=$STATUS"
  case "$STATUS" in done|error|cancelled) break ;; esac
  sleep 5
done

# 5. Closing deletes the on-server clone
curl -sk -X DELETE "$SERVER/v1/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY"
```

**Using `awman` itself:**

```sh
SESSION=$(awman remote session start --type remote \
  --repo-url https://github.com/org/my-project --branch main \
  | awk '/ created via / {print $2}')

awman remote exec workflow aspec/workflows/implement-feature.toml \
  --work-item 0053 --session "$SESSION" --follow

awman remote session kill "$SESSION"
```

`awman remote session start` does not set `base_ref`, `merge_refs` or `push`; use the HTTP API for those.

---

## One-shot scripted execution (`exec`)

The `exec` subcommand group provides two commands for running agent tasks non-interactively from scripts, CI pipelines, or the API server — without a persistent session or TUI.

### `awman exec prompt [<prompt>]`

Launches an agent container with a pre-supplied prompt. Behaves identically to `awman chat`, except the initial prompt is baked into the launch arguments rather than requiring a live terminal session.

```sh
# Run a single task and exit
awman exec prompt "Fix the failing tests in src/api"

# Non-interactive: agent executes and exits; output goes to stdout
awman exec prompt "Summarise recent changes" --non-interactive

# Use a specific agent and model
awman exec prompt "Refactor the auth module" --agent codex --model gpt-4o

# Send a GitHub issue as the prompt
awman exec prompt --issue 84

# Prepend your own context to the issue
awman exec prompt "Security review for: " --issue 84

# Full autonomous run
awman exec prompt "Implement caching for the API layer" --yolo --non-interactive
```

A prompt, `--issue`, or both is required. With both, your text comes first, followed by the issue content ([GitHub Integration](10-github-integration.md)).

`exec prompt` accepts the same agent flags as `chat` (`--non-interactive`/`-n`, `--plan`, `--auto`, `--yolo`, `--agent`, `--model`, `--overlay`, `--agent-flag`, `--launch-mode`, `--allow-docker`); see [Agent Sessions](03-agent-sessions.md#flags-common-to-chat-and-other-agent-launching-commands) and the [Command Reference](14-command-reference.md#awman-exec-prompt).

---

### `awman exec workflow <path>` / `awman exec wf <path>`

Runs a workflow file. The work item is optional — when provided, it's used for template variable substitution within the workflow.

```sh
# Run a workflow without a work item
awman exec workflow ./aspec/workflows/implement-feature.toml

# Alias: exec wf
awman exec wf ./aspec/workflows/implement-feature.toml

# Associate a local work item for template variable substitution
awman exec workflow ./aspec/workflows/implement-feature.toml --work-item 0053

# Or fetch a GitHub issue instead
awman exec workflow ./aspec/workflows/implement-feature.toml --issue 84

# Non-interactive workflow run
awman exec workflow ./aspec/workflows/review.toml --non-interactive
```

`exec workflow` and `exec wf` are identical — `wf` is a short alias.

**Work item template variables:** When no `--work-item` or `--issue` is given, prompt templates that use `{{work_item_number}}`, `{{work_item_content}}`, or `{{work_item_section:[Name]}}` are left unexpanded with a warning:

```
warning: workflow uses {{work_item_content}} but no --work-item was provided; placeholder left unexpanded
```

With `--work-item <N>`, awman resolves the work item file from the configured work items directory and substitutes all template variables; `--issue <ref>` fetches the GitHub issue and substitutes the same variables.

Workflow state is saved under `$GITROOT/.awman/workflows/` as `<repo-hash8>-<work-item>-<workflow-name>.json`, or `<repo-hash8>-<workflow-name>.json` without a work item.

All workflow flags (`--worktree`, `--yolo`, `--dynamic`, `--on-existing-worktree`, the prerequisite flags, `--json`, …) are described in [Workflows](05-workflows.md#flags) and listed in the [Command Reference](14-command-reference.md#awman-exec-workflow).

---

## Non-interactive (headless) operation

Both `exec` and the API server run **non-interactively** — without expecting input from a terminal. There are no interactive prompts, dialogs, or pauses for confirmation; the agent runs from start to finish and you observe the result through stdout/stderr or the event stream. This is what makes awman safe to drive from CI/CD pipelines, cron jobs, and scripts.

### Automatic TTY detection

For the one-shot CLI (`awman chat`, `exec prompt`, `exec workflow`), awman checks whether stdin is connected to a terminal and switches to non-interactive mode automatically when it is not:

```sh
# Interactive (TTY detected)
awman chat

# Non-interactive (stdin redirected from a pipe or file)
awman chat < /dev/null

# Non-interactive (explicit flag, even when a TTY is present)
awman chat --non-interactive
```

When a TTY is present and `--non-interactive` is not passed, awman allows interactive features — full-screen agent sessions, PTY passthrough, and terminal resize (SIGWINCH) forwarding. When no TTY is detected, the agent runs in print/batch mode and awman makes no attempt to manage the terminal.

The API server (`awman api start`) is **always** non-interactive, regardless of the server process's stdin: clients drive it over HTTP and receive updates through the event stream, so there is no terminal to manage.

### Parallel agents in interactive CLI mode

`exec workflow` can run more than one step at once when the workflow's steps are independent (see [Parallel workflows](05-workflows.md#parallel-workflows)). In command mode, running multiple agents at once with a real terminal attached would normally mean several containers writing to your terminal at the same time. Instead, when stdout is a TTY and more than one step is running, awman activates a lightweight status chrome:

- The bottom row of the terminal is reserved for a status line: `[N agents running | showing: <step_name> | Ctrl-S: switch]`
- The rest of the terminal shows the output of whichever step is currently "showing" (the **focused** step)
- Press **Ctrl-S** to cycle the focused step; the display switches to that step's output and the status line updates
- A stuck focused step (no output for 30 seconds) gets a `⚠` prefix on its name in the status line

This chrome is deliberately minimal — a single reserved status row, not a full UI — and only appears when there is something to arbitrate between. With a single running step, or when stdout is not a TTY (redirected to a file, piped, or `--non-interactive`), awman falls back to plain sequential passthrough exactly as described above. In that non-interactive case, if a workflow launches more than one step in parallel, awman prints a warning that step output will interleave without any chrome to separate it, since there is no terminal to reserve a status row on.

### The `--non-interactive` / `-n` flag

Force non-interactive mode even when a TTY is available. Useful for:

- Testing how a workflow behaves headlessly without leaving your terminal.
- Scripting awman commands inside a shell script you happen to be running interactively.
- Environments with a quirky terminal where TTY detection is unreliable.

### Yolo mode in headless contexts

`--yolo` works without a terminal, and its stuck detection keeps unattended runs from stalling: when a step produces no output for the stuck timeout (`agentStuckTimeout`, default 30 seconds), a 60-second countdown starts, reported on stderr every 10 seconds, and the step advances when it expires.

```sh
# CI: no explicit --non-interactive needed; awman detects the headless context
awman exec workflow aspec/workflows/feature-impl.toml --work-item 0042 --yolo
```

The API applies the same `yolo` auto-advance behavior as the CLI.

### Error handling and exit codes

When something fails in a non-interactive run, awman:

1. **Prints error details to stderr** — step name, error code, and any container output.
2. **Exits non-zero** — so CI/CD detects the failure and stops the pipeline.
3. **Does not attempt interactive recovery** — no retry prompts, no manual intervention.

This makes results easy to integrate: check the exit code and act on it. All output goes to stdout (step output, status messages) and stderr (errors, yolo countdown), so you can capture and redirect freely:

```sh
awman exec workflow workflow.toml --yolo > workflow.log 2>&1
```

### Terminal-dependent agents

An agent that needs full-screen terminal control (e.g. an interactive editor) requires a real TTY and cannot run headlessly. Either refactor the workflow to avoid full-screen features, run it on a machine with a terminal, or provide a virtual TTY (tmux/screen) inside the container — rarely needed.

### Headless runs on the builtin runtime

Headless, CI, and API-server runs work the same way under `runtime: "builtin-experimental"` (macOS on Apple Silicon only), with these differences:

- The host must be an Apple Silicon Mac running the signed `awman-macos-arm64` binary (a macOS CI runner or VM must itself expose Hypervisor.framework). No Docker daemon is needed to run agents, but images must be available: run `awman ready` (with a configured `builtin.imageSource`) as a setup step. Unless `builtin.builder.enabled` is set with a local source, a launch whose image is missing fails with build-and-import instructions; workflow preflight and squad never build.
- Each session's microVM lives and dies with the awman process that launched it. Under `awman api start`, that is the API server; stopping the server stops the VMs it launched.
- `awman ready --json` includes an `image_source` entry when images were imported.

See [Runtimes: Builtin microVM](11-runtimes.md#builtin-microvm-experimental).

---

## Starting the server

### Foreground

```sh
awman api start --port 9876 --workdirs /path/to/repo
```

The server listens on `127.0.0.1` at the given port (default `9876`) and accepts requests for the life of the process. The first start generates an API key and prints it in a banner — copy it then; it isn't shown again. Subsequent starts reuse the stored hash silently. By default the server speaks HTTPS with a self-signed cert; pass `--dangerously-skip-tls` for plain HTTP in trusted-local use. Logs go to stderr; press `Ctrl+C` to stop.

```sh
# Multiple working directories
awman api start --workdirs /repo-a --workdirs /repo-b

# Custom port
awman api start --port 8080 --workdirs /repo

# Rotate the API key and print the new one to stdout (does not start the server)
awman api start --refresh-key --port 9876 --workdirs /repo

# Plain HTTP on loopback for tests/dev (WARNING: no TLS)
awman api start --dangerously-skip-tls --port 9876 --workdirs /repo

# Disable authentication for this run (WARNING: anyone reachable can drive the server)
awman api start --dangerously-skip-auth --port 9876 --workdirs /repo
```

`--workdirs` accepts one or more absolute paths (repeat the flag for multiple values). `--workdir-root` (also repeatable) names a directory whose direct-child Git repositories are allowed. Only working directories on the allowlist can be used to create sessions — requests with any other path are rejected with HTTP 403. See [Working directory allowlist](#working-directory-allowlist).

**Start flags related to authentication and transport:**

| Flag | Description |
|------|-------------|
| `--refresh-key` | Generate a new API key, write its hash to disk, and print the plaintext key, then exit without starting the server. Stop a running server first (`awman api kill`); the old key stops working when the server next starts. |
| `--dangerously-skip-auth` | Disable authentication for this process lifetime only. The `api_key.hash` file is left untouched; the next normal start re-enables auth using the stored hash. |
| `--dangerously-skip-tls` | Serve plain HTTP instead of HTTPS. Intended for tests and trusted-local environments only; the server logs a warning at startup. Clients must connect with `http://`. |

### Reaching the server from another machine

The server binds to `127.0.0.1` only; there is no flag to listen on another interface. To use it from another machine, forward the port over SSH (for example `ssh -N -L 9876:127.0.0.1:9876 build-host`) or put a reverse proxy on the server host in front of it.

If `awman remote` cannot connect to a non-loopback address, its error says so and suggests the tunnel:

```
awman: remote connection refused: …; the awman API server listens on 127.0.0.1 only, so https://build-host:9876 must be an SSH tunnel (`ssh -N -L 9876:127.0.0.1:9876 <server-host>`, then use https://127.0.0.1:9876) or a reverse proxy running on the server host
```

When `awman remote` targets a loopback address, it trusts the certificate at `~/.awman/api/tls/cert.pem` on the *client* machine, so through an SSH tunnel copy the server's `cert.pem` there. For a non-loopback address (a proxy), the proxy must present a certificate the client trusts.

### Background

```sh
awman api start --background --port 9876 --workdirs /path/to/repo
```

`--background` daemonizes the server using the OS process manager:

| Platform | Mechanism |
|----------|-----------|
| Linux (systemd available) | `systemd-run --user` transient unit |
| macOS (launchd) | `~/Library/LaunchAgents/io.awman.api.plist`, loaded with `launchctl` |
| Fallback | Detached child process |

The PID is stored at `~/.awman/api/awman.pid`. Logs go to `~/.awman/api/awman.log`.

If a server is already running, `start` fails with `API server already running on PID <pid>`. If the port is taken by something else, it fails with `Port 9876 is already in use. Use --port to choose a different port.`

### API server and squad daemon

The API server and the squad daemon share the database and cannot run at the
same time. Starting either one while the other is alive fails before the new
process starts. The error names the other process and its PID, for example:

```
error: the squad daemon is already running (PID 41290). Run `awman squad stop` first; awman api and the squad daemon cannot run at the same time.
```

Stop the running process before starting the other one. See [Squad and
`awman api`](12-squad.md#squad-and-awman-api) for the equivalent instructions
from the squad user's perspective.

---

## Server lifecycle commands

### Status

```sh
awman api status
```

Prints whether the server is running, its PID and address, and whether it answers an HTTP probe:

```
API server is running (PID 81234) at https://127.0.0.1:9876.
```

When it is not running: `API server is not running.` For session and command counts, call [`GET /v1/status`](#server-health).

### Logs

```sh
awman api logs
```

Prints the contents of `~/.awman/api/awman.log`, which is written only when the server was started with `--background` (use `tail -f` on that file to follow it). If the file does not exist, awman says `Log file not found: <path>`.

#### Session-setup verbosity

While a session is being prepared (`cloning_repository`, `setting_up_branch`, `merging_refs`, `running_ready`), the server writes the full ready output — including container build lines, git command output, and state transitions — to the API log file. Each line is prefixed with the first 8 characters of the session id so you can grep one session out of a busy log:

```sh
grep '\[a1b2c3d4\]' ~/.awman/api/awman.log
```

Session setup runs the same `ready` image step as `awman ready`: under the builtin runtime it builds an image only when `builtin.builder.enabled` is true and the image is missing, and otherwise imports (re-acquiring an image installed from a different source than the one now configured). Each session uses the builtin settings of its own repo.

To silence this verbose stream, set `AWMAN_API_VERBOSE_SETUP=0` before starting the server. The lines are then emitted at `debug` level and dropped by the default `info` filter; they remain available via `RUST_LOG=debug`.

### Kill

```sh
awman api kill
```

Stops the background server (see [Graceful shutdown](#graceful-shutdown)) and removes its PID file, printing `API server (PID <pid>) stopped.` When no server is running it prints `No API server is running (no PID file found).` (or notes a stale PID file it removed) and exits 1.

---

## Authentication

The API server uses cryptographic API key authentication. Every HTTP request must include the API key; unauthenticated requests are rejected with HTTP 401 before reaching any handler.

### First start — key generation

On the first start (when no `api_key.hash` file exists in `~/.awman/api/`), awman generates a random 32-byte key (64 hex characters), stores only its SHA-256 hash on disk, and prints the key once:

```
╔═══════════════════════════════════════════════════════════════════╗
║  awman API key (store this — it will not be shown again)          ║
║  a3f8b2c1...64-character-hex-key...d7e9f0a1                       ║
╚═══════════════════════════════════════════════════════════════════╝
```

**This is the only time the plaintext key appears.** Store it immediately. With `--background`, the key is printed by the starting command, before the daemon and its log file exist, so it never appears in `awman.log`. In the TUI and over the API the key is stated as plain text instead of a box.

### Subsequent starts

On subsequent starts (when `~/.awman/api/api_key.hash` already exists), the server loads the stored hash silently and starts normally — no banner is printed. The same key continues to work without any client-side changes.

### Rotating the key — `--refresh-key`

To invalidate the current key and generate a new one:

```sh
awman api start --refresh-key --port 9876 --workdirs /repo
```

A new key is generated, its hash replaces the file on disk, and the key is printed in the same banner. The command then exits without starting the server; it refuses to run while a server is running. Clients using the old key receive HTTP 401 from the next server start and must be updated.

### Disabling authentication — `--dangerously-skip-auth`

```sh
awman api start --dangerously-skip-auth --port 9876 --workdirs /repo
```

Skips all authentication checks for this process lifetime. The `api_key.hash` file is not modified; the next normal start re-enables authentication. Use only in isolated, trusted environments (e.g. a local loopback-only setup with strict firewall rules).

### Authenticating requests

Include the API key in every HTTP request using the `Authorization` header:

```sh
# Bearer token format (recommended)
curl -s http://localhost:9876/v1/status \
  -H "Authorization: Bearer <your-api-key>"

# Bare key format (also accepted)
curl -s http://localhost:9876/v1/status \
  -H "Authorization: <your-api-key>"
```

When using `awman remote` subcommands, the key is resolved and injected automatically — see [API key](#api-key-authentication).

**Error responses from the middleware:**

| Situation | HTTP status | Response body |
|-----------|-------------|---------------|
| No `Authorization` header | 401 | `{"error": "API key required. Pass the key via the Authorization header (e.g. Authorization: Bearer <key>).", "code": "unauthorized"}` |
| Wrong key | 401 | `{"error": "Invalid API key.", "code": "unauthorized"}` |

The `Bearer` prefix is matched case-insensitively. Hashes are compared in constant time, and a request with no header costs the same as one with a wrong key.

### Key storage

The key hash is stored at `~/.awman/api/api_key.hash` with mode `0o600` (owner read/write only on Unix). Only the SHA-256 hex digest is written — the plaintext key is never persisted anywhere.

---

## Working directory allowlist

The server maintains a strict allowlist of working directories. Any session creation request that specifies a path not on the allowlist is rejected with HTTP 403.

There are two kinds of entry:

- **Workdirs** name one directory each. Sources: `--workdirs` flags passed to `awman api start` and `api.workDirs` in the global config (`~/.awman/config.json`).
- **Roots** name a directory that *contains* repositories. Sources: `--workdir-root` flags (repeatable) and `api.workDirRoots` in the global config. A request is accepted when its `workdir` is a repository cloned **directly** beneath a root — so a client can clone `/srv/repos/app` and create a session on it without restarting the server.

All sources of each kind are merged. A request is allowed if it matches any workdir entry or any root.

```sh
awman api start --workdir-root /srv/repos
awman config set --global api.workDirRoots "/srv/repos,/home/user/projects"
```

A directory under a root is accepted only when all of these hold:

1. It is a direct child of the root — not a grandchild, and not the root itself.
2. It is its own Git root (its top-level directory is the child itself, not a parent repository).
3. Its `.git` is a real directory. A linked worktree (where `.git` is a file) or a symlinked `.git` is refused.

A refused request returns HTTP 403 with a message naming the rule that failed and the path as submitted. Use `GET /v1/workdirs` for the effective allowlist.

A root must not be a Git repository or lie inside one: starting the server with one exits with code 2 and suggests `--workdirs` for a single repository. This condition is checked again on requests, including for roots that appear after startup. Git discovery errors refuse the root. Exact workdir entries must not contain a root or its ancestors: such a mount would let one session replace another session's checkout directory. Roots must remain host-owned.

Every path is resolved to its canonical form (symlinks resolved, trailing slashes stripped) via `std::fs::canonicalize`. Roots are re-resolved on every request, so a root created after startup works. If a root does not exist at startup, a warning is logged and it remains configured for later requests. Exact workdir entries must already exist. Background startup preserves absolute paths and the invoking working directory, including when relative entries are read again from global config.

To see the current allowlist over HTTP:

```sh
curl http://localhost:9876/v1/workdirs
```

```json
{
  "workdirs": [
    "/home/user/my-project",
    "/home/user/other-project"
  ],
  "roots": [
    "/srv/repos"
  ]
}
```

`roots` is an empty array when none are configured.

---

## Job Queue

The API server uses a per-session FIFO queue to serialize command execution. When you submit a command via `POST /v1/commands`, it is enqueued immediately and returns with a `command_id`. A pool of worker tasks processes the queue: each worker claims one queued command at a time and executes it, then claims the next one.

- **One command per session at a time**, in submission order, so commands in a session don't interfere with each other.
- **Concurrency across sessions**: with N workers, up to N sessions run a command at once. Set N with the global `workers` setting (default 2; see [Server configuration](#server-configuration)).
- **Submission does not wait for execution**: a valid command for a ready session is enqueued with HTTP 202. `POST /v1/commands` returns 409 if the session is closing or its setup is not ready; `POST /v1/runs` can queue its command during setup.

Track progress with `GET /v1/commands/{id}`, or inspect a session's queue depth, running command and recent completions with [`GET /v1/sessions/{id}/queue`](#get-session-queue-status).

---

## HTTP API

All endpoints speak JSON. All requests and responses are logged at `INFO` level or above.

When authentication is enabled (the default), every request must include the API key in the `Authorization` header. See [Authentication](#authentication) for details.

### Base URL

```
https://localhost:<port>/v1        # default (self-signed TLS)
http://localhost:<port>/v1         # only if started with --dangerously-skip-tls
```

The default TLS cert is self-signed (stored at `~/.awman/api/tls/cert.pem`), so HTTP clients will reject it unless you tell them not to. With curl:

```sh
# Easiest: skip verification (acceptable for loopback)
curl -k https://localhost:9876/v1/status -H "Authorization: Bearer $KEY"

# Stricter: pin the self-signed cert as the only trusted CA
curl --cacert ~/.awman/api/tls/cert.pem https://localhost:9876/v1/status \
  -H "Authorization: Bearer $KEY"
```

Every `curl` example in the rest of this document elides `-k`/`--cacert` for brevity; add one of the two flags above to any of them when running against the default HTTPS server.

### Endpoint reference

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/workdirs` | List the server's allowlisted working directories (`workdirs`) and workdir roots (`roots`) |
| `POST` | `/v1/sessions` | Create a new session |
| `GET` | `/v1/sessions` | List sessions; accepts optional `status`, `setup_status` and repeatable `label` filters |
| `GET` | `/v1/sessions/:id` | Get session detail |
| `GET` | `/v1/sessions/:id/status` | Session setup progress (`status` becomes `ready` when commands can be submitted) |
| `GET` | `/v1/sessions/:id/queue` | Get queue status for a session (queued, running, recent completed) |
| `DELETE` | `/v1/sessions/:id` | Close a session |
| `POST` | `/v1/commands` | Submit a subcommand to a session (enqueued) |
| `GET` | `/v1/commands` | List commands; filter by `session_id`, `status`, `label` |
| `POST` | `/v1/commands/:id/cancel` | Cancel a queued or running command |
| `POST` | `/v1/runs` | Create a session and run a command in one request |
| `GET` | `/v1/commands/:id` | Get command status and metadata (`/v1/commands/:id/status` returns the same body) |
| `GET` | `/v1/commands/:id/logs` | Stream structured command events via Server-Sent Events; `?format=json` returns the captured events instead |
| `GET` | `/v1/commands/:id/workflow` | Get the workflow state for a command (`/v1/workflows/:id` returns the same body) |
| `GET` | `/v1/commands/:id/outputs` | List files the agent wrote under `/awman/output` |
| `GET` | `/v1/commands/:id/outputs/*path` | Download one output file |
| `GET` | `/v1/status` | Server health (uptime, active sessions, running commands, and supported `features`) |

### Agent commands

Commands submitted through `POST /v1/commands` expose an agent snapshot and
management operations across the configured runtime, independent of the selected
session's working directory:

| Subcommand | API availability |
|------------|------------------|
| `agents status` | Returns a one-time status snapshot. `--watch` is not available through the API. |
| `agents list` | Lists agents. |
| `agents detail <id>` | Returns details for the selected agent. |
| `agents kill <id>` | Stops and removes the agent; include `--yes` because API requests cannot answer an interactive confirmation. |
| `agents attach <id>` | Not available through the API. |

For example, set `subcommand` to `agents status` or `agents kill` and pass
arguments such as the agent ID and `--yes` in the request's `args` array. The
result is the same JSON payload as the CLI's `--json` output, emitted as
one `StdoutLine` event and stored in the command's `result.data`. The same
commands are documented in [Monitoring running agents](03-agent-sessions.md#monitoring-running-agents).

---

### Sessions

#### Create a session

There are two types of sessions:

**Local session** — bound to a working directory on the server's filesystem:

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"local","workdir":"/home/user/my-project"}'
```

**Remote session** — clones a git repository on the server and operates within the clone:

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"remote","repo_url":"https://github.com/org/repo","branch":"main"}'
```

`session_type` defaults to `local`. Both types return HTTP 202 immediately with a session UUID:

```json
{ "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890" }
```

Setup (cloning, for a remote session, then the same checks and image preparation as `awman ready`) continues in the background. Commands can be submitted once `GET /v1/sessions/:id/status` reports `ready`; until then `POST /v1/commands` returns HTTP 409 `session_not_ready`. [`POST /v1/runs`](#one-shot-runs) queues a command without waiting.

**Remote session options** — a remote session body can also carry these optional fields, all of which require `branch`:

```json
{
  "session_type": "remote",
  "repo_url": "https://github.com/org/repo",
  "branch": "feature/login-fix",
  "base_ref": "release/2.4",
  "merge_refs": ["feature/shared-types", "feature/api-client"],
  "push": true
}
```

| Field | Meaning |
|-------|---------|
| `branch` | The session branch. If it exists on `origin`, the session continues it; otherwise it is created. |
| `base_ref` | Branch, tag or commit the new session branch is created from. Defaults to `origin`'s default branch. If `branch` already exists on `origin`, setup fails (omit `base_ref` to continue it). |
| `merge_refs` | Branch names on `origin`, merged into the session branch in order before the session becomes `ready`. |
| `push` | When `true`, the session branch is committed and pushed to `origin` after each successful `exec` workflow or prompt command. Default `false`. |

`base_ref`, `merge_refs` and `push` are only valid on remote sessions (a local session returns HTTP 400), and each requires `branch` (HTTP 400 otherwise). A `branch` or `merge_refs` entry that is not a valid git branch name, or a malformed `base_ref`, is also rejected with HTTP 400. Merge targets reject `HEAD`, hexadecimal commit spellings (including abbreviated SHAs), and names advertised only as tags. A name that exists as both a branch and a tag selects the branch. An entirely missing ref fails during setup.

**Setup stages.** `GET /v1/sessions/:id/status` reports the stage the session is in: `initializing`, `cloning_repository`, `setting_up_branch`, `merging_refs`, `running_ready`, then `ready` (or `failed`). The `merging_refs` stage runs only when `merge_refs` is non-empty; each ref is merged as `origin/<ref>` in order, and progress is reported per ref. If a ref does not exist on `origin`, or a merge conflicts, setup fails, the error names the stage (for a conflict it lists the conflicting paths), and the clone is deleted. Merge commits use your git identity (`user.name` / `user.email`) when configured, and a built-in awman identity otherwise.

**Pushing results.** With `push: true`, after an `exec prompt` or `exec workflow` command (including dynamic workflows) exits successfully, awman commits any leftover changes and pushes `HEAD` to `refs/heads/<branch>` on `origin`, setting an upstream and never forcing. Its workflow state JSON and saved dynamic TOML files are excluded even when already staged; their working files are preserved. Other repository files, including Dockerfiles generated during setup, are eligible leftovers. A fresh branch with no new work is not published; setup merge commits are published even if the command makes no edits. If `origin` already has the same commit, no second push is needed. A successful publication, including a workflow's own push during this command, is recorded as `pushed: {"remote": "origin", "branch": "...", "sha": "..."}`; a no-op reports `pushed: null`. Failed, aborted or paused commands are not automatically pushed.

Commands share the clone's working state. Edits left by a failed command remain available for repair and are included if a later successful command leaves them in the clone. Workflow teardown runs before the automatic leftover commit: use `commit_changes` then `push_branch` before `create_pull_request` when the PR must already include those edits. A later automatic push updates the same branch.

If the push fails (for example the branch moved on `origin`, or the credentials are rejected), the command ends with status `error`, exit code 1 and `stage: "push"`, with git's error text included. Local commits stay in the clone; nothing is retried or forced.

The push runs on the server as the user running `awman api`, using that user's git credentials (SSH keys, credential helpers). Make sure that account can push to `repo_url`. Host push-back uses isolated Git metadata with hooks and repository-configured helpers disabled; the destination comes from the session's saved URL. Each host push-back Git operation times out after two minutes.

**Local sessions** bind to an existing directory on the server. The directory must be in the allowlist (an `api.workDirs` / `--workdirs` entry, or a Git repository directly beneath an `api.workDirRoots` / `--workdir-root` root). Commands run against that directory; an `exec workflow` with `--worktree` (implied by the API's default `--yolo`) runs in a worktree of it under `~/.awman/worktrees/`.

**Remote sessions** clone a git repository into `<API root>/sessions/<session_id>/<repository-folder>` on the server. The API root is selected from `AWMAN_API_ROOT`, `AWMAN_CONFIG_HOME/api`, `XDG_DATA_HOME/awman/api`, then `~/.awman/api`, in that order. Commands run inside the clone. Closing the session deletes the clone; command logs remain in API storage. This is useful for running ephemeral CI jobs without a local checkout.

#### End-to-end example: remote session with setup and teardown

The server clones the repository when the session is created; the workflow's setup installs dependencies, its steps implement the feature, and teardown tests, commits, pushes and opens a pull request.

**1. Create a remote session and wait for `ready`:**

```sh
SESSION=$(curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"remote","repo_url":"https://github.com/org/my-project","branch":"main"}' \
  | jq -r .session_id)

until [ "$(curl -s http://localhost:9876/v1/sessions/$SESSION/status \
  -H 'Authorization: Bearer <api-key>' | jq -r .status)" = ready ]; do sleep 5; done
```

**2. The workflow file, committed in the repository as `aspec/workflows/implement-feature.toml`:**

```toml
name = "implement-feature"
teardown_on_failure = true

[[setup]]
type = "run_shell"
command = "npm install"

[[steps]]
name = "implement"
prompt = """
Analyze the following work item and implement the feature:

{{work_item_content}}
"""

[[steps]]
name = "review"
depends_on = ["implement"]
prompt = "Review the implementation for correctness and style."

[[teardown]]
type = "run_shell"
command = "npm test"

[[teardown]]
type = "commit_changes"
message = "feat: implementation from automated workflow"

[[teardown]]
type = "push_branch"

[[teardown]]
type = "create_pull_request"
title = "feat: auto-implemented feature"
base = "main"
```

API commands default to `--yolo`, which implies `--worktree`, so the run already happens on its own branch; `push_branch` with no fields pushes that branch.

**3. Submit and follow it:**

```sh
CMD=$(curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec workflow","args":["aspec/workflows/implement-feature.toml","--work-item","0053"]}' \
  | jq -r .command_id)

curl -N http://localhost:9876/v1/commands/$CMD/logs -H 'Authorization: Bearer <api-key>'
```

Notes:

- Setup and teardown steps run in containers with the session's checkout mounted, like agent steps. Because `teardown_on_failure = true`, teardown runs even if a step fails.
- If the workflow is resumed, the whole setup phase runs again, so setup steps should be idempotent.
- `commit_changes`, `push_branch` and `create_pull_request` need a git identity, push credentials and a GitHub token, and `gh` in the base image. A headless run with an unresolved prerequisite fails before launch by default, so grant them in `args` (for example `"--prerequisite-token","env:GITHUB_TOKEN"`); see [Workflow prerequisite checks](05-workflows.md#workflow-prerequisite-checks).
- When the command finishes, `result.pull_request.url` names the pull request it opened.
- Closing the session deletes the clone; command records and logs are kept.

Session-creation error responses:

| Situation | HTTP status |
|-----------|-------------|
| `workdir` not in allowlist (local) | 403 — names the failed rule and includes the allowed workdirs and roots |
| Invalid `session_type` field | 400 |
| Missing required fields | 400 |
| `base_ref`, `merge_refs` or `push` on a local session, or without `branch` | 400 |
| Invalid branch or ref name | 400 |
| Network error cloning repo (remote) | Create returns 202; asynchronous setup reports `failed` on the session status endpoint |

#### List sessions

```sh
# All sessions (active and closed)
curl -s http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>'

# Active sessions only
curl -s "http://localhost:9876/v1/sessions?status=active" \
  -H 'Authorization: Bearer <api-key>'
```

The optional `status` query parameter filters by session status. Accepted values: `active`, `closing`, `closed`. The repeatable `label` parameter filters by [label](#labels) (`label=k=v` or `label=k`; all must match). The optional `setup_status` parameter filters by setup stage (`initializing`, `cloning_repository`, `setting_up_branch`, `merging_refs`, `running_ready`, `ready` or `failed`) and combines with `status` and `label`; for example `setup_status=failed` finds remote sessions whose clone or merge failed. An invalid value returns HTTP 400 `invalid_request`. Without parameters, all sessions are returned.

Every session record carries `setup_status`. When it is `failed`, the record also carries `setup_error: {"stage": "...", "message": "..."}` (omitted if no error was recorded).

The same readiness fields appear on `GET /v1/sessions/:id` and all JSON responses of `DELETE /v1/sessions/:id`. DELETE returns the session record when closed; while draining it returns a closing acknowledgement (202 initially, 200 if already closing) with `session_id`, `status`, readiness, `running_command_id`, `cancelled_count` and `message`.

```json
{
  "sessions": [
    {
      "id": "a1b2c3d4-...",
      "workdir": "/home/user/my-project",
      "status": "active",
      "created_at": "2026-04-20T12:00:00Z",
      "closed_at": null,
      "labels": { "ci-run": "4812" },
      "setup_status": "ready"
    },
    {
      "id": "b2c3d4e5-...",
      "workdir": "/home/user/my-project",
      "status": "closed",
      "created_at": "2026-04-19T09:00:00Z",
      "closed_at": "2026-04-19T09:47:13Z",
      "labels": {},
      "setup_status": "ready"
    }
  ]
}
```

#### Get session detail

```sh
curl -s http://localhost:9876/v1/sessions/<session-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the session record, including its `labels`. It also includes `setup_status` (and `setup_error` when setup failed), so a client can tell whether the session is ready for commands without polling `/status`. For remote sessions this includes `repo_url`, `branch`, `base_ref`, `merge_refs` (omitted when empty) and `push`. Returns HTTP 404 if the ID does not exist.

#### Close a session

```sh
curl -s -X DELETE http://localhost:9876/v1/sessions/<session-id> \
  -H 'Authorization: Bearer <api-key>'
```

Gracefully closes a session. All queued commands are cancelled immediately, but any currently running command is allowed to finish before the session is marked closed.

**Behavior:**
- **No queued commands:** Session transitions immediately to `closed` status. Returns HTTP 200.
- **Queued commands, no running command:** All queued commands are cancelled. Session transitions immediately to `closed` status. Returns HTTP 200.
- **Running command:** The session transitions to `closing` status, rejecting new submissions. The running command is allowed to finish. Returns HTTP 202 with details:
  ```json
  {
    "session_id": "a1b2c3d4-...",
    "status": "closing",
    "running_command_id": "cmd-456",
    "cancelled_count": 3,
    "message": "Session is closing. Waiting for running command to complete. Poll GET /v1/sessions/{id}/status to monitor."
  }
  ```
  Poll `GET /v1/sessions/{id}/status` to observe the transition to `closed`.

Closed sessions cannot receive new commands. All command records and output files are preserved — no data is deleted. Returns HTTP 404 if the session does not exist.

---

### Commands

Commands are submitted to a session and execute asynchronously. Submit a command and receive a `command_id` immediately; poll the command endpoint to track progress and retrieve output.

#### Submit a command

```sh
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"<session-id>","subcommand":"exec prompt","args":["hello"]}'
```

Dispatches a subcommand to the session named by the `session_id` field in the JSON body. The `x-awman-session` header is still supported as an alternative; if both are sent they must name the same session, otherwise the request is rejected with HTTP 400 `invalid_request`. Idempotency comparison covers whichever form was sent.

The API accepts these commands: `exec prompt`, `exec workflow`, `agents status|list|detail|kill` (see [Agent commands](#agent-commands)), and the `squad` task and daemon commands except `squad attach` (which needs an interactive PTY) and `squad env`. Anything else — `chat`, `ready`, `init`, `config`, `new`, `clean`, `remote` — is refused with HTTP 400 `invalid_request` ("command not available via API"), and the response lists the available commands. The [Command Reference](14-command-reference.md) marks each command's frontends.

For `exec`, put the action in `subcommand` (`exec prompt` or `exec workflow`) and the action's arguments in `args`. A bare `"subcommand": "exec"` with the action as the first `args` entry is rejected at admission with HTTP 400 `invalid_request`:

```sh
# exec prompt via API
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"<session-id>","subcommand":"exec prompt","args":["Fix the failing tests","--non-interactive"]}'

# exec workflow via API
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"<session-id>","subcommand":"exec workflow","args":["./aspec/workflows/implement-feature.toml","--work-item","0053","--prerequisite-unresolved","warn"]}'
```

Workflow prerequisite options are passed in the same `args` array as CLI
flags: `--prerequisite-identity`, `--prerequisite-ssh`,
`--prerequisite-token`, and `--prerequisite-unresolved`. Use
`--check-prerequisites` to return the prerequisite report without starting the
workflow. API commands are non-interactive: flags can grant a specific choice,
but the API caller cannot silently widen the configured credential policy.
Set global `workflows.prerequisites.allowApiClientGrants` to `true` only if API
clients should be allowed to grant credentials beyond the normal policy.

**Workflow files:** For `exec workflow`, the workflow file path is specified as a positional argument relative to the session's working directory (same format as the CLI). The file must exist within the session's working directory and be a valid TOML or YAML workflow definition. Workflow files are never passed as inline JSON — always use file paths.

Invalid prerequisite syntax and unsupported `secret:NAME` return HTTP 400 at admission. Credential-widening requests return HTTP 403 unless globally authorized. Accepted responses include the secret-free resolved policy in `flags_applied.prerequisites`.

Returns immediately with a command UUID — the command is **enqueued** and execution is asynchronous:

```json
{
  "command_id": "e5f6a7b8-...",
  "flags_applied": { "yolo": true, "non_interactive": true }
}
```

The command begins executing as soon as a worker claims it from the queue. **At most one command per session runs at any given time** — commands within a session are processed sequentially in FIFO order. If a command is already running, subsequent commands are queued and wait their turn (they do not block the API request).

**`flags_applied`** reports the API's flag defaults for the submitted command. It lists only flags the command accepts, so it is `{}` for commands such as `agents list`:

| Flag | Behavior |
|------|----------|
| `non_interactive` | Always `true`. API workers have no terminal, so `exec prompt` and `exec workflow` always run in print/batch mode. You never need to pass `--non-interactive`. |
| `yolo` | `true` by default. It is a default, not an override: a `--yolo` value in `args` is kept, and `--plan` or `--auto` in `args` suppresses it. See [Permission modes](03-agent-sessions.md#permission-modes). |

For `exec workflow`, `flags_applied.prerequisites` also carries the resolved credential policy (see above).

Error responses:

| Situation | HTTP status | `code` |
|-----------|-------------|--------|
| Session not found or closed | 404 | `session_not_found` |
| Session is closing (graceful shutdown) | 409 | `session_closing` |
| Session setup still running or failed | 409 | `session_not_ready` |
| Unknown subcommand | 400 | `unknown_subcommand` |
| Unknown flag | 400 | `unknown_flag` |
| No session named in the body or the `x-awman-session` header, or the two disagree | 400 | `invalid_request` |

See [Error responses](#error-responses) for the full list of codes.

#### Get command status

```sh
curl -s http://localhost:9876/v1/commands/<command-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current status and metadata for a command. `GET /v1/commands/{id}/status` is still served and returns the identical body.

```json
{
  "id": "e5f6a7b8-...",
  "session_id": "a1b2c3d4-...",
  "subcommand": "exec workflow",
  "args": ["deploy.toml"],
  "status": "queued",
  "queued_at": "2026-04-20T12:00:30Z",
  "queue_position": 2,
  "labels": {}
}
```

| `status` value | Meaning |
|----------------|---------|
| `queued` | Enqueued; waiting for a worker to claim it |
| `running` | Claimed by a worker; container is executing |
| `done` | Completed with exit code 0 |
| `error` | Completed with a non-zero exit code |
| `cancelled` | Cancelled by [`POST /v1/commands/:id/cancel`](#cancel-a-command), or before execution by a session kill. Exit code 130 when it had started |

**`log_path` (deprecated).** Responses to requests from a loopback address (`127.0.0.0/8` or `::1`) also carry `log_path`, the path of the command's output log on the server. It is omitted when the TCP peer is not loopback, and is deprecated: read output through `/v1/commands/{id}/logs` and `/v1/commands/{id}/outputs`. The address checked is the immediate TCP peer; forwarding headers are ignored. SSH tunnels and proxies running on the API host connect from loopback, so their clients can also receive this field. This check does not establish that the original client is local.

**Other response fields:** `labels` is always present (empty object when none). `cancel_requested_at` appears once running cancellation has been requested and is retained on terminal reads. Optional fields are omitted when unavailable.

**Queue-related response fields** (present when `status = 'queued'`):

| Field | Description |
|-------|-------------|
| `queued_at` | ISO 8601 timestamp when the command was enqueued |
| `queue_position` | 0-indexed position in the session's queue (0 = next to run). Omitted after leaving the queue |
| `worker_id` | ID of the worker task that claimed this command. Retained after completion |

**Result field** (present when `status = 'done'`, `'error'` or `'cancelled'`):

```json
{
  "exit_code": 0,
  "outcome": "done",
  "git": {
    "dirty": false,
    "changed_files": [],
    "changed_files_truncated": false
  },
  "outputs": { "files": 0, "bytes": 0 }
}
```

`result` is the structured description of how the command ended. Optional
fields are omitted when they do not apply:

| Field | Meaning |
|-------|---------|
| `exit_code` | Process or command exit code, when known |
| `outcome` | `done`, `error`, `aborted`, `paused` or `cancelled`; refines the command row's status |
| `failure` | Why a command did not finish successfully: `{ "stage", "code", "message", "step?" }` |
| `failed_steps` | Failed workflow steps, each with its name, phase, index, last `step_attempt`, and known exit code or error |
| `git` | Checkout summary: branch and base/head SHAs when known, dirty flag, and changed-file details |
| `worktree_path` | Retained worktree path, when applicable |
| `pull_request` | `{ "url": "…" }` for a PR opened by a successful `create_pull_request` teardown step |
| `final_message` | The final agent message for `exec prompt` when the selected agent provides structured output |
| `outputs` | `{ "files", "bytes" }` summary of files under `/awman/output` |
| `data` | JSON data for commands with a JSON result |
| `error` | Legacy error text; ordinary failures use `failure.message`, while setup and cancellation retain their legacy markers |
| `cancelled_at` | Time an accepted cancellation was recorded |
| `pushed` | Push result for sessions configured with `push: true`; may be `null` when no push occurred |

Git details apply to commands that execute in a checkout. They are omitted for
queued cancellations and an existing-worktree preflight refusal. Changed files
include uncommitted and untracked edits, are capped at 500 entries, and set
`changed_files_truncated` when entries are omitted. Binary files have null line
counts. If diff collection fails, `changed_files_error` explains why the list
is unavailable while known branch, head and dirty facts remain available.
A step that succeeds on retry is removed from `failed_steps`.

`error_status`, `stage` and `setup_error` are deprecated compatibility fields,
filled exactly as before `failure` existed. Use `failure.code` and
`failure.stage` instead of `error_status` and `stage`; use `failure` for
session setup errors instead of `setup_error`. The legacy `error_status` is a
numeric HTTP-style error status; `stage` is set only for an unresolved workflow
prerequisite (`workflow_prerequisites`), a push failure (`push`) and a
`/v1/runs` setup failure (`setup`), and is absent for every other failure and
for cancellations, whereas `failure.stage` is always present; `setup_error`
holds the `/v1/runs` setup details. The legacy
`error` field remains available as the human-readable message. A command row
status is only `queued`, `running`, `done`, `error` or
`cancelled`; `aborted` and `paused` are values of `result.outcome`, not row
statuses.

**Failure codes.** `failure.stage` is one of `setup`, `preflight` (after
admission, before any container started), `workflow_prerequisites`, `step`,
`teardown`, `merge` or `push`. `failure.code` is a stable snake_case code; match
on it rather than on `failure.message`. Codes describing how a run ended:

| `failure.code` | Meaning |
|----------------|---------|
| `step_failed` | A workflow main step or the `exec prompt` agent failed (`failure.step` names the step when one is to blame) |
| `teardown_failed` | The main phase finished but a teardown step failed |
| `command_failed` | A non-execution command (for example `agents status`) exited non-zero |
| `push_failed` | The automatic push of a `push: true` session failed |
| `setup_failed` | `/v1/runs` session setup, or an aborting workflow setup step, failed |
| `cancelled` | The command was cancelled (`outcome: "cancelled"`) |
| `aborted` | The workflow was aborted, for example by a step with `abort_on_failure` (`outcome: "aborted"`) |
| `paused` | The workflow paused and can be resumed (`outcome: "paused"`) |
| `invalid_queue_record` | The stored command could not be decoded, so it never ran |
| `internal` | An unclassified failure inside awman, its runtime or its storage |

Any other failure has one code per error condition. Where the [error-code
table](#error-responses) already names a condition, the same code is used
(`unknown_subcommand`, `unknown_flag`, `forbidden_workdir`, `worktree_exists`,
`merge_conflict`, `merge_target_dirty`, `workflow_prerequisites_unresolved`,
`unknown_agent`, `agent_untrusted`). The rest are named after the condition, for
example `work_item_not_found`, `workflow_file_not_found`,
`spec_template_missing`, `invalid_overlay_spec`, `missing_required_argument`,
`invalid_flag_value`, `agent_not_found`, `agent_ambiguous` or
`not_implemented`. An HTTP error returned at admission keeps its own class
code from the error-code table (`invalid_request`, `not_found`, …) for
conditions that table does not name.

**Malformed `args`:** `POST /v1/commands` and the nested command in `POST /v1/runs` validate flags and positional arguments against the command catalogue before enqueueing. Unknown flags or invalid typed values return HTTP 400 without creating a command, for example:

```json
{
  "error": "unknown flag 'not-a-real-flag' for command [\"exec\", \"prompt\"]",
  "code": "unknown_flag"
}
```

Execution failures after admission are recorded in the command's `result`; poll `GET /v1/commands/{id}` or follow its log stream.

#### Get a command log snapshot

```sh
curl -s 'http://localhost:9876/v1/commands/<command-id>/logs?format=json' \
  -H 'Authorization: Bearer <api-key>'
```

Returns a snapshot of the captured event log, including the command's current
attempt. For a running command it contains events persisted so far; for a
completed command it contains the retained events from every attempt.

```json
{
  "session_id": "…",
  "command_id": "…",
  "attempt": 1,
  "events": [
    {
      "timestamp": "…",
      "attempt": 1,
      "sequence": 0,
      "payload": { "type": "StdoutLine", "data": "…" }
    }
  ]
}
```

The default form of this endpoint is the live SSE stream described below.

#### Stream command logs (live)

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/logs \
  -H 'Authorization: Bearer <api-key>'
```

Opens a persistent HTTP response using [Server-Sent Events (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events). The server replays persisted events, then sends new events as they arrive. A terminal `event: done` frame means the command really ended: the command status is `done`, `error`, or `cancelled`. The server sends it only after the terminal status is recorded, then closes the response. It does not send `event: timeout`; a worker wait or a quiet command does not mean the command ended.

**SSE event format:**

```
id: 1:0
event: stdout_line
data: {"timestamp":"…","attempt":1,"sequence":0,"payload":{"type":"StdoutLine","data":"<line of output>"}}

id: 1:1
event: stderr_line
data: {"timestamp":"…","attempt":1,"sequence":1,"payload":{"type":"StderrLine","data":"<line of output>"}}

id: 1:2
event: done
data: {"timestamp":"…","attempt":1,"sequence":2,"payload":{"type":"Done"}}

```

Each SSE frame is terminated by a blank line. Persisted events carry `id: <attempt>:<sequence>`; sequence numbers start at 0 and increase within each attempt. The JSON payload is typed, and the `event:` name identifies its type. Treat `event: done` as the completion signal. Clients that ignore `id:` and continue reading `event:` and `data:` work as before.

Every event JSON object may also include a `source` envelope field describing
where it came from. For attributed work it contains `phase` (`leader`,
`setup`, `main`, `teardown`, `merge` or `push`), and may contain `step_name`,
`step_index`, `container` and 1-based `step_attempt`. The step index is its
position in the workflow definition; `step_attempt` counts retries of that
step. The envelope's `attempt` separately counts command executions after a
daemon restart. Queue notices and terminal `command_status`/`done` events have
no source. Shell setup and teardown output and leader output may have a phase
without a step name.

**Shell example — stream and print until done:**

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/logs \
  -H 'Authorization: Bearer <api-key>' \
| while IFS= read -r line; do
    case "$line" in
      "event: done") echo "--- done ---"; break ;;
      data:\ *)              echo "${line#data: }" ;;
    esac
  done
```

**Behaviour notes:**

- The informational waiting event uses `attempt: 0` and `sequence: 0`; it is not a persisted event or a resume position.
- Normal completion drains buffered stdout/stderr before the final workflow completion notifications and `done`. If a backend leaves an output pipe open, the server waits up to five seconds, logs a warning, and completes; trailing output may be missing in that exceptional case.
- API stdout/stderr are published as lines. Ordinary text and stderr lines retain at most a 64 KiB prefix; if a line is longer, the excess is skipped through the next newline and the emitted line ends with ` [line truncated]`. Invalid UTF-8 is decoded lossily, and a final nonempty line without a newline is still emitted. Structured stdout frames are accepted up to 8 MiB; an oversized frame produces only a bounded activity hint, not its raw contents.
- If the command has already completed when you connect, the server replays its persisted events and sends `event: done` before closing.
- If the client disconnects mid-stream, the command continues executing unaffected.
- An unknown command ID returns HTTP 404. A queued command with no log yet stays connected while it waits for a worker.
- The `Content-Type` response header is `text/event-stream`.
- While a command waits for a worker, the server sends an id-less `status_message` event saying it is waiting, then an SSE comment (`: waiting for command progress`) every 15 seconds. A quiet live stream also gets an id-less `: keepalive` comment every 15 seconds. Comments do not advance the resume cursor. There is no server-side worker-wait deadline and no `event: timeout`; HTTP client timeouts are separate from command completion. With cURL, use `-N` to display frames as they arrive and `--max-time 0` to disable cURL's overall timeout for long-running commands.
- `attempt` is the 1-based execution number for the command. After a restart, a command whose completion was recorded before the interruption is finalized without being dispatched again. A command interrupted before completion is re-queued; its next execution has a higher `attempt` number and sequence numbering starts again at 0. Its first persisted event is a `status_message` with phase `queue` and message `re-queued after restart (attempt N)`. If the server cannot read required recovery data, startup fails with the storage error rather than risking a duplicate side effect; fix or remove the damaged recovery file named by the error before restarting. `events.log` appends every attempt, preserving earlier events.

**Resume a stream:** pass the last persisted event ID as `Last-Event-ID` or as the `after` query parameter. The server replays persisted events strictly after that cursor; for a terminal command it also sends the terminal `done` frame so the client can observe completion. This terminal `done` is sent even when the supplied cursor equals or exceeds its ID, so clients should accept a repeated completion notification. If both resume inputs are provided, `?after=` takes precedence. Since earlier attempts remain in `events.log`, a cursor from an earlier attempt is valid and replay continues after it into later attempts. If the cursor's attempt is not present in the log, is 0, or is newer than the command's current attempt, replay restarts at the beginning of the current attempt with a comment such as `: resuming from the start of attempt 2`. A malformed effective cursor returns HTTP 400. A valid query cursor overrides even a malformed `Last-Event-ID` header.

```sh
# Resume after the last frame previously received, e.g. id: 2:17
curl -N -H 'Authorization: Bearer <api-key>' \
  -H 'Last-Event-ID: 2:17' \
  'http://localhost:9876/v1/commands/<command-id>/logs'

# Equivalent query form (takes precedence if the header is also present)
curl -N -H 'Authorization: Bearer <api-key>' \
  'http://localhost:9876/v1/commands/<command-id>/logs?after=2:17'
```

`awman remote exec --follow` uses this endpoint internally. The cURL form above is equivalent and is useful in scripts where the awman binary is unavailable on the client.

Add `?step=<step-name>` to the live logs URL or JSON snapshot URL to receive only events attributed
to that workflow step. Command-level completion events (`command_status` and
`done`) are still sent so the filtered stream can finish on its own. For
example:

```sh
curl -N 'http://localhost:9876/v1/commands/<command-id>/logs?step=implement' \
  -H 'Authorization: Bearer <api-key>'
```

**Structured events:** each event frame carries a named `event:` type (`stdout_line`,
`stderr_line`, `status_message`, `workflow_prerequisites`, `workflow_step_transition`,
`workflow_phase_transition`, `workflow_parallel_step_launched`,
`workflow_parallel_step_exited`, `workflow_parallel_group_finished`,
`command_status`, `step_activity`, `done`) and a JSON `data:` payload, so a client that wants
structured data instead of log lines can read the `event:` field and parse
accordingly. Add `?format=json` to the same URL to fetch the event-log snapshot
described above. It includes all retained attempts and their historical `Done`
records, including a marker written immediately before a crash and re-queue.
Interpret those records by `attempt`; only the SSE `done` notification certifies
that the current command is terminal. Resume inputs do not filter this snapshot.

The worker emits exactly one final `command_status` event after recording the
terminal command row, followed by `done`. Its `result` is the same structured
object returned by the command status endpoint. The event's status can be
`paused` or `aborted` even though the persisted row status is `error`; inspect
`result.outcome` for that distinction. The older `status`, `exit_code` and
`error` event fields remain for compatibility; the event's `error` is exactly
`result.error`, so a push failure no longer has a `push: ` prefix there (its
`result.failure.stage` is `push`).

The stream waits for delayed final log writes. If the log cannot be recovered,
it sends completion from the terminal row without event IDs, preserving the
persisted cursor space for a later reconnect.

Structured-output agents can also produce `step_activity` events. Their
payload is `{ "type": "StepActivity", "kind": "message" | "tool_use" |
"tool_result" | "thinking", "summary": "…" }`. The summary is a bounded
one-line hint (at most 200 characters), and the event's `source` attributes it
to its step. Activity events supplement ordinary stdout/stderr output.

Final messages and activity summaries currently have the same redaction limits
as log text; do not assume they conceal secrets. Any future log redaction must
also cover those fields. Valid structured frames with unknown content produce
bounded activity hints rather than raw tool payloads; malformed JSON lines pass
through as ordinary output.

The status-carrying events use small typed enums rather than free-form
strings, so a client can match on a fixed set of values:

| Field | Values |
|-------|--------|
| `workflow_step_transition.from_status` / `.to_status` | `pending`, `running`, `succeeded`, `failed`, `cancelled`, `skipped` |
| `workflow_phase_transition.status` | `running`, `succeeded`, `failed`, `paused`, `teardown_failed`, `cancelled` |
| `command_status.status` | `done`, `paused`, `aborted`, `error`, `cancelled` |

`workflow_phase_transition` is emitted for the main workflow and for each
setup or teardown phase step. Its `phase` is `setup`, `main`, or `teardown`,
`step_desc` identifies the phase step, and `status` reports its transition.
Setup and teardown transitions let clients track those phases independently of
the main `workflow_step_transition` events.

`workflow_prerequisites` carries a `WorkflowPrerequisites` report with its
phase (`start` or `before_teardown`) and check results. The start report is
emitted before workflow step events; a second report is emitted before
non-cancelled teardown when awman rechecks granted credentials. If prerequisite
policy requires a missing check to fail, the command ends with HTTP status 409
and error stage `workflow_prerequisites`; the response includes the report so
the client can show what needs attention.

#### Get session queue status

```sh
curl -s http://localhost:9876/v1/sessions/<session-id>/queue \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current queue state for a session — how many commands are queued, which one is running, and recently completed commands.

```json
{
  "session_id": "a1b2c3d4-...",
  "queue_depth": 3,
  "running": {
    "command_id": "cmd-456",
    "subcommand": "exec workflow",
    "args": ["deploy.toml"],
    "started_at": "2026-04-20T12:05:00Z",
    "worker_id": "worker-789"
  },
  "queued": [
    {
      "command_id": "cmd-457",
      "subcommand": "exec workflow",
      "args": ["test.toml"],
      "queued_at": "2026-04-20T12:01:00Z",
      "position": 0
    },
    {
      "command_id": "cmd-458",
      "subcommand": "exec prompt",
      "args": ["review the code"],
      "queued_at": "2026-04-20T12:02:00Z",
      "position": 1
    }
  ],
  "recent_completed": [
    {
      "command_id": "cmd-455",
      "subcommand": "exec workflow",
      "args": ["setup.toml"],
      "status": "done",
      "exit_code": 0,
      "finished_at": "2026-04-20T12:00:00Z"
    }
  ]
}
```

**Response fields:**

| Field | Description |
|-------|-------------|
| `session_id` | The session UUID |
| `queue_depth` | Total number of enqueued (not yet running) commands |
| `running` | The currently executing command, or `null` if none |
| `queued` | Array of enqueued commands, in FIFO order, with 0-indexed position |
| `recent_completed` | Array of the 10 most recent completed commands (status `done`, `error` or `cancelled`), newest first |

The `running`, `queued` and `recent_completed` entries each include a `labels` object.

This endpoint is useful for monitoring queue depth and diagnosing stalls in multi-command workflows.

#### Cancel a command

```sh
curl -s -X POST http://localhost:9876/v1/commands/<command-id>/cancel \
  -H 'Authorization: Bearer <api-key>'
```

Stops a command that has not finished. No request body or `x-awman-session` header is needed. What happens depends on the command's state:

| Command is… | Response | Effect |
|-------------|----------|--------|
| Queued | `200` with the command's status object | Cancelled immediately; it never starts. |
| Running | `202` | The command is told to stop. Its containers are stopped and removed, then the command finishes as `cancelled`. |
| Already finished (`done`, `error`, `cancelled`) | `200` with the command's status object | Nothing changes; the response shows the final state. |
| Unknown ID | `404` `{"error":"Command '<id>' not found"}` | — |

The `202` response means "stopping", not "stopped":

```json
{
  "command_id": "e5f6a7b8-...",
  "session_id": "a1b2c3d4-...",
  "status": "running",
  "cancel_requested_at": "2026-04-20T12:06:00Z",
  "message": "Stopping. Poll GET /v1/commands/{id}/status until status is cancelled."
}
```

Poll `GET /v1/commands/<command-id>` until `status` is `cancelled`. Repeating the cancel while the command is still stopping returns `202` again and keeps the original `cancel_requested_at`. A command that finishes cleanly before the stop takes effect stays `done`; the cancel does not rewrite a completed result.

A cancelled command has `status: "cancelled"` and `exit_code: 130`. Its `result` has `outcome: "cancelled"`, `failure.code: "cancelled"` (with the stage it was cancelled in) and the legacy `error: "cancelled"`. A command cancelled while running also carries `cancelled_at`.

**Cancellation is not a failure.** A workflow's `on_failure` retry and remediation handling is not triggered, and `abort_on_failure` is not applied. For a workflow command that is cancelled partway through:

- Teardown still runs, so **cleanup steps are kept**.
- Teardown **skips the publishing steps**: `push_branch`, `create_pull_request` and `poll_ci`. For a session created with `push: true`, the automatic push of the session branch is skipped too.
- A cancel that arrives during teardown or push-back still ends the command `cancelled`. Publishing steps that have not started are skipped; a push already in progress cannot be undone. A command that finished before the cancellation request keeps its result.

After a daemon restart, a recorded cancellation stays cancelled. The daemon stops surviving command-owned agents and finishes interrupted cleanup before starting queue workers or deleting a one-shot run's clone. Completed cleanup entries are retained; a cleanup entry interrupted by the crash may run again, so cleanup commands should be idempotent. Setup, main steps and remediation are not relaunched during this recovery.

`exec prompt` (including ACP agents), `exec workflow` and dynamic workflows are all cancellable. Closing a session with `DELETE /v1/sessions/:id` still lets a running command finish; use the cancel endpoint first if you want it stopped.

#### List and retrieve command outputs

API-backed agent runs can write files under `/awman/output`. List the files
for a command with `GET /v1/commands/{id}/outputs`; fetch one with
`GET /v1/commands/{id}/outputs/{path}`:

Persistent outputs require a runtime that supports host directory mounts
(Docker, Apple Containers or builtin). Sandbox runtimes do not mount or
advertise `/awman/output`; the command emits a warning about this limitation.
On Unix hosts each file is opened one path component at a time without
following symbolic links. On Windows hosts awman checks paths instead: every
component must be a plain directory or regular file (symbolic links and
junctions are refused), the resolved path must stay inside the outputs
directory, and Windows device names (`NUL`, `CON`, `COM1`, …) and names ending
in a dot or a space are neither listed nor served.

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/outputs \
  -H 'Authorization: Bearer <api-key>'

curl -s http://localhost:9876/v1/commands/<command-id>/outputs/report.json \
  -H 'Authorization: Bearer <api-key>'
```

The listing returns `files` with relative `path`, `size`, `sha256` and
`modified_at` fields, plus a `total` `{ "files", "bytes" }` summary. The same
totals appear in `result.outputs`. A missing output directory is an empty
listing, and listing is available while a command is running. File retrieval
returns the file body with a content type inferred from its extension (or
`application/octet-stream`); files larger than 16 MiB return HTTP 413. Invalid
paths return 400, and unknown commands or missing files return 404.

Responses are downloads, including HTML and SVG, with a scriptless sandbox
policy. A listing that is busy, exceeds its work budget (100,000 directory
entries, 256 MiB of hashing, or two seconds), or observes a file changing returns
503; retry later. At most two listings run concurrently, and disconnected
requests stop their work. Totals count all regular files, including names that
cannot be addressed through a URL. If summary traversal cannot finish,
`result.outputs.incomplete: true` marks its totals as a lower bound.

Files are stored under the command's API directory and are removed with that
command's logs. The output directory has no hard quota: it has the same disk
exposure as the workspace. Files are not redacted, so agents should not write
secrets or credentials there.

`exec prompt` clears its outputs at the start of each execution attempt,
including a restart. Resumed workflows retain existing outputs; rerun steps
overwrite their own files. Parallel steps share the directory, so collisions
are last-writer-wins; use a subdirectory named after the step.

#### List commands

```sh
curl -s "http://localhost:9876/v1/commands?label=job=nightly&status=running" \
  -H 'Authorization: Bearer <api-key>'
```

| Query parameter | Meaning |
|-----------------|---------|
| `session_id` | Only commands in this session |
| `status` | One of `queued`, `running`, `done`, `error`, `cancelled`. The old value `pending` is still accepted as an alias for `queued` |
| `label` | A [label](#labels) selector, `k=v` or `k`. Repeat the parameter to require several; all must match |
| `limit` | Maximum results; default 100, clamped to 1–1000. A non-integer returns HTTP 400 |

Returns `{"commands":[...]}`, newest first. Each entry has the same shape as `GET /v1/commands/:id`. An unknown `status` or malformed selector returns HTTP 400.

---

### Error responses

Idempotent replays preserve the originally stored response bytes. Errors cached by a server version before the `error_codes` feature may lack `code` until the key expires (24 hours).

New error responses are JSON with a human-readable `error` message and a stable, machine-readable `code`:

```json
{ "error": "Session 'a1b2c3d4-...' not found", "code": "session_not_found" }
```

Match on `code`, not on the `error` text. Existing messages are preserved for compatibility. Some errors carry extra fields (for example `hint`, `session_id`, `setup_status` or `blocked_command`). Errors from the HTTP framework itself — malformed JSON, an unsupported content type, an oversized body, an unknown route or a wrong method — use the same shape and retain their original HTTP status, with the codes below.

| `code` | HTTP status | Meaning |
|--------|-------------|---------|
| `unauthorized` | 401 | The `Authorization` header is missing or the key is wrong |
| `forbidden_workdir` | 403 | The session `workdir` is not allowed by the [working directory allowlist](#working-directory-allowlist) |
| `invalid_request` | 400 | The request is malformed or fails validation: bad JSON, an invalid query value, an invalid label selector or `Idempotency-Key`, no session named, or a body `session_id` that disagrees with the `x-awman-session` header |
| `forbidden` | 403 | The requested operation is forbidden, including credential widening |
| `not_found` | 404 | An unknown route or missing admission resource |
| `method_not_allowed` | 405 | The route does not accept this HTTP method; see the `Allow` header |
| `request_too_large` | 413 | The request body exceeds the limit |
| `output_not_found` | 404 | The output file is unavailable |
| `output_too_large` | 413 | The output exceeds the download limit |
| `unsupported_media_type` | 415 | The request content type is unsupported |
| `unprocessable_request` | 422 | The JSON cannot be decoded into the required request type |
| `unknown_subcommand` | 400 | The `subcommand` is not a known command (a known command that is not available over the API is `invalid_request`) |
| `unknown_flag` | 400 | `args` contains a flag the command does not accept |
| `session_not_found` | 404 | The session does not exist, or is closed |
| `session_closing` | 409 | The session is closing and rejects new commands |
| `session_not_ready` | 409 | The session's setup has not finished or has failed |
| `command_not_found` | 404 | No command with that ID exists |
| `no_workflow` | 404 | The command did not run a workflow |
| `workflow_not_started` | 404 | The workflow command has not produced a snapshot yet; retry |
| `workflow_never_started` | 404 | The workflow command ended without producing a snapshot; stop polling |
| `idempotency_conflict` | 422 | The `Idempotency-Key` was already used for a different request |
| `idempotency_in_flight` | 409 | The original request with this `Idempotency-Key` is still being handled; sent with `Retry-After: 1` |
| `internal` | 500 | The server failed to complete the request |
| `unavailable` | 503 | Output listing is temporarily unavailable; retry after `Retry-After: 1` |
| `worktree_exists` | 409 | A workflow worktree for this work item already exists |
| `merge_conflict` | 409 | Merging the workflow branch conflicted |
| `merge_target_dirty` | 409 | The merge target has uncommitted changes |
| `workflow_prerequisites_unresolved` | 409 | Workflow credential prerequisites could not be resolved |
| `unknown_agent` | 404 | The requested agent is not defined |
| `agent_untrusted` | 403 | The agent definition has not been trusted |

`unknown_subcommand`, `unknown_flag`, `forbidden_workdir` and the last six codes are the same values reported asynchronously in a command's `result.failure.code`, so one decoder serves both. Several of them can also be returned synchronously when a request is rejected at admission. `invalid_request`, `forbidden` and `not_found` are HTTP class codes only: a command that fails after admission reports the specific condition instead (see [Failure codes](#get-command-status)). A run whose setup actually fails uses `setup_failed`; submitting a new command to an unready session uses `session_not_ready`, because that command has not been admitted.

---

### Labels

Sessions and commands carry optional string labels, set at creation and returned on every read. Use them to tag work with your own identifiers (a job name, a pull request number) and to find it again later.

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"remote","repo_url":"https://github.com/org/repo","branch":"main",
       "labels":{"ci-run":"4812","team":"payments"}}'
```

`POST /v1/sessions`, `POST /v1/commands` and `POST /v1/runs` accept `labels` as a JSON object of strings (default `{}`). Responses that describe a session or command — `GET` detail, status and list, `GET /v1/sessions/:id/queue` entries, and the `DELETE` response — always include a `labels` object, empty when there are none.

**Limits.** At most 16 labels per object. Each key and value is at most 256 UTF-8 bytes. Keys cannot be empty or contain `=`. Neither keys nor values may contain control characters. Empty values are allowed. Anything else returns HTTP 400 with a validation message.

**Filtering.** `GET /v1/sessions` and `GET /v1/commands` accept a repeatable `label` query parameter:

- `label=team=payments` matches objects whose `team` label is exactly `payments`.
- `label=team` matches objects that have a `team` label with any value.
- Repeated selectors are ANDed.

`GET /v1/sessions` also accepts `status=active|closing|closed`.

**Example: reattach after a restart.** A CI wrapper that tagged its work can recover after it crashes or is rescheduled, without having stored any IDs:

```sh
# Is there still a running command for this CI run?
curl -s "http://localhost:9876/v1/commands?label=ci-run=4812&status=running" \
  -H 'Authorization: Bearer <api-key>' | jq -r '.commands[0].id'

# Or find the session, then its commands
curl -s "http://localhost:9876/v1/sessions?label=ci-run=4812&status=active" \
  -H 'Authorization: Bearer <api-key>' | jq -r '.sessions[0].id'
```

Once you have the command ID, resume with `GET /v1/commands/<id>/logs` or the status endpoint, or stop it with the cancel endpoint.

---

### Idempotency keys

Network failures leave a client unsure whether a create request arrived. `POST /v1/sessions`, `POST /v1/commands` and `POST /v1/runs` accept an optional `Idempotency-Key` header so the request can be retried safely:

```sh
curl -s -X POST http://localhost:9876/v1/runs \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Idempotency-Key: ci-4812-attempt-1' \
  -H 'Content-Type: application/json' \
  -d @run.json
```

The key is 1–255 visible ASCII characters; anything else returns HTTP 400. Keys are remembered for 24 hours, per route and per API key (or shared when authentication is disabled). Two requests are the same when their JSON bodies are equivalent; for `POST /v1/commands` the selected session is part of the comparison, whether it was sent as the body `session_id` or the `x-awman-session` header (the same command and session replays either way).

| Retry with the same key… | Result |
|--------------------------|--------|
| After the original completed, same body | The original status and body are replayed, with the header `Idempotent-Replayed: true`. No second session or command is created |
| Same key, different body | HTTP 422 `idempotency_conflict` |
| While the original is still being handled | HTTP 409 `idempotency_in_flight` with `Retry-After: 1` (HTTP 422 `idempotency_conflict` if the body also differs) |
| After the original returned a 5xx | Treated as a new request; nothing was stored |

Responses below 500 are replayed, including 4xx errors. If the daemon restarts before the client received the response, a retry still gets the original `202` when the resource had already been created; otherwise it is processed as new. Use a fresh key for each distinct piece of work.

---

### One-shot runs

#### Start a run

`POST /v1/runs` creates a session and submits a command in a single request. It replaces the sequence create session → poll `GET /v1/sessions/:id/status` until `ready` → `POST /v1/commands` → `DELETE /v1/sessions/:id`.

```sh
curl -s -X POST http://localhost:9876/v1/runs \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{
    "session": {
      "session_type": "remote",
      "repo_url": "https://github.com/org/repo",
      "branch": "main",
      "labels": { "ci-run": "4812" }
    },
    "command": {
      "subcommand": "exec workflow",
      "args": ["./aspec/workflows/implement-feature.toml", "--work-item", "0053"],
      "labels": { "ci-run": "4812" }
    },
    "close_on_finish": true
  }'
```

Response (HTTP 202):

```json
{ "session_id": "a1b2c3d4-...", "command_id": "e5f6a7b8-..." }
```

`session` takes the same fields as [Create a session](#create-a-session) and `command` the same fields as [Submit a command](#submit-a-command) except `session_id` (400 `invalid_request` if supplied: the run creates and selects its own session), and both are validated before anything is created (HTTP 400, 403 or 404 as for those routes). The command is queued immediately and starts on its own once session setup reaches `ready`, so there is no readiness check and no HTTP 409 for an unready session. `Idempotency-Key` is supported.

Track the run with `GET /v1/commands/<command_id>` and `GET /v1/commands/<command_id>/logs`.

**`close_on_finish`** (default `false`). When `true`, the session is closed as soon as the command reaches a terminal state — `done`, `error` or `cancelled` — and a remote session's clone is deleted. This also applies when setup fails or the run is cancelled before the command starts. Logs and command records are kept. With `false`, the session stays open for further commands.

**Setup failure.** If session setup fails (a clone error, a merge conflict, a failing ready step), the queued command never runs. It becomes `error` with exit code 1 and a structured failure:

```json
{
  "exit_code": 1,
  "outcome": "error",
  "failure": {
    "stage": "setup",
    "code": "setup_failed",
    "message": "..."
  },
  "error": "..."
}
```

For compatibility, current responses can also include deprecated
`error_status`, `stage` and `setup_error` fields. Use `failure` in new clients.

**Cancelling during setup.** `POST /v1/commands/<command_id>/cancel` works while setup is still running: setup is aborted, the command ends `cancelled`, and the session is closed and any remote clone deleted — whatever `close_on_finish` is set to.

---

### Server health

```sh
curl -s http://localhost:9876/v1/status \
  -H 'Authorization: Bearer <api-key>'
```

```json
{
  "status": "ok",
  "pid": 12345,
  "uptime_seconds": 11640,
  "active_sessions": 2,
  "running_commands": 1,
  "features": ["error_codes", "workflow_state", "command_cancel", "labels", "idempotency", "runs", "structured_results", "outputs", "event_source", "step_activity", "command_record", "session_setup_status", "command_session_body"]
}
```

The `features` array lists capabilities clients can detect without relying on
the server version. `workflow_state` means the server exposes live workflow
snapshots through `GET /v1/commands/:id/workflow`.
`command_cancel`, `labels`, `idempotency` and `runs` mean the server supports [command cancellation](#cancel-a-command), [labels](#labels), [idempotency keys](#idempotency-keys) and [`POST /v1/runs`](#one-shot-runs).
`error_codes` means every error body carries a `code` ([Error responses](#error-responses)). `structured_results` means command records carry the structured [`result`](#get-command-status); `outputs` means the [outputs endpoints](#list-and-retrieve-command-outputs) exist; `event_source` means events carry `source` and the logs accept `?step=`; `step_activity` means `step_activity` events can appear. `command_record` means `GET /v1/commands/:id` is served. `session_setup_status` means sessions carry `setup_status` and `setup_error` and `GET /v1/sessions` accepts `setup_status`. `command_session_body` means `POST /v1/commands` accepts `session_id` in the body.

New capabilities are added to `features`, never removed, so check for a feature rather than comparing versions.

---

### Workflow state

API-mode `exec workflow` commands launch available steps automatically and
finish when the last step completes. A failed step is retried once, then fails
the command (`error`); a step with `abort_on_failure` aborts it. No interactive
confirmation is required.

When a workflow starts, the API server snapshots its `WorkflowState` to
`workflow.state.json` in that command's API directory. The snapshot is updated
atomically as the state changes. For an ordinary workflow, the first snapshot
exists before setup runs. With `--dynamic`, it exists after the leader's
workflow design validates and before setup runs. The local engine state is
also stored under `$GITROOT/.awman/workflows/`. Snapshot write errors are logged
without failing the command; the last successfully written snapshot remains.
On Windows, readers should close their file handles between reads so sharing
locks do not prevent replacement. Command status remains authoritative.

For worktree merge-back, `--on-dirty-merge-target` accepts `ask`, `commit`,
`stash`, or `fail`; `--on-merge-failure` accepts `ask`, `agent`, or `keep`.
API headless defaults are `fail` for a dirty merge target and `keep` after a
merge failure. See [Worktree merge-back](05-workflows.md#worktree-merge-back)
for what each choice does. These policies apply at final merge-back. The API
still automatically commits changes already present before creating a worktree;
start from a clean checkout to avoid that existing preparation behavior.

#### Existing worktrees

When the worktree for a run already exists, an API `exec workflow` **fails by
default**: the command ends with status `error`, exit code 1 and HTTP 409, and
the message names the worktree path and branch. Nothing on disk is changed, and
this applies even when a saved workflow state would otherwise be resumed. Pass
`--on-existing-worktree` in the command's arguments to choose instead:

- `resume` continues in the existing worktree and branch.
- `recreate` deletes the worktree, its branch and every commit on that branch,
  then starts fresh.
- `fail` is the default.

See [Worktrees](05-workflows.md#worktrees). Worktrees live under
`~/.awman/worktrees/<repo-name>-<hash>/` on the server. A retained checkout is reported as `result.worktree_path` in the completed command. This includes failures after worktree preparation; a checkout removed after a successful squash merge is omitted. Resuming verifies that the checkout's Git pointer and registration still belong to this repository.

#### Get workflow state

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/workflow \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current workflow view for the given command. The older path
`GET /v1/workflows/{command_id}` is still served and returns the same body. The response retains
the persisted `WorkflowState` fields and adds client-ready fields at read time;
it does not change the local workflow state file.

```json
{
  "schema_version": 4,
  "invocation_id": "d2b50c8e-5bd0-4da6-9e19-0ed469d33f6d",
  "workflow_name": "implement-feature",
  "workflow_hash": "a41f…",
  "work_item": 132,
  "steps": [
    { "name": "plan", "depends_on": [], "agent": "codex", "model": "gpt-6", "index": 0, "stage": 0, "status": "succeeded", "step_attempt": 1 },
    { "name": "docs", "depends_on": ["plan"], "index": 1, "stage": 1, "status": "running", "step_attempt": 1 }
  ],
  "stages": [["plan"], ["docs"]],
  "step_states": {
    "plan": "Succeeded",
    "docs": { "Running": { "container_id": null } }
  },
  "completed_steps": ["plan"],
  "current_step_index": 1,
  "started_at": "2026-10-27T12:00:00Z",
  "updated_at": "2026-10-27T12:05:00Z",
  "phase": "main",
  "current_phase": "Main",
  "setup_completed": true,
  "teardown_completed": false,
  "setup_step_states": [
    { "description": "run_shell: prepare", "status": "Succeeded", "state": "succeeded" }
  ],
  "teardown_step_states": [
    { "description": "run_shell: verify", "status": "Pending", "state": "pending" }
  ]
}
```

The response retains `step_states` and adds `stages`, an array grouping step
names by dependency stage. Each `steps` entry adds its definition-order
`index`, dependency `stage`, snake_case `status` (`pending`, `running`,
`succeeded`, `failed`, `cancelled` or `skipped`), and `step_attempt` after the
step has launched. Failed steps may also include `exit_code` and `error`. Once
a step has launched, `container` is the runtime container name of its latest
launch: the same value events carry in `source.container`, kept after the step
finishes. A running step also includes `container_id`, the runtime's
container ID. Other top-level fields such as `step_attempts` and
`step_containers` are the engine's own bookkeeping; read the per-step fields
instead. Setup and teardown progress remains
in `setup_step_states` and `teardown_step_states`, whose entries gain a
snake_case `state` (`pending`, `running`, `succeeded`, `failed`, `remediating`
or `skipped`). The added top-level `phase` is snake_case (`setup`, `main`,
`teardown` or `done`).

The legacy `current_phase`, `step_states`, and phase-entry `status` fields
remain in their persisted representation for compatibility. `WorkflowState`
has no top-level workflow status property; monitor command status or the final
`command_status` event for command completion.
The legacy enum values use Serde's externally tagged representation: unit
variants are strings such as `"Succeeded"`, and data variants are one-key
objects, as in `{"Running":{"container_id":null}}` or
`{"Failed":{"exit_code":1,"error_message":"step failed"}}`. Legacy
phase-entry `status` values are `"Pending"`, `"Running"` (without a
container ID), `"Succeeded"`, `{"Failed":{"error":"reason"}}`, or
`{"Remediating":{"attempt":1,"of":2}}`; agent steps additionally use
`"Cancelled"` and `"Skipped"`. There is no derived workflow-level status;
monitor command status or the final `command_status` event for command
completion.

Always also watch `GET /v1/commands/{command_id}` or `command_status` events.
A command can end before its workflow starts, or fail while the most recent
snapshot still shows a phase short of Done. A Done workflow snapshot also
precedes worktree merge-back, whose failure can still fail the command.
On restart, completed work is retained, but interrupted steps can reset to
Pending and phase steps can rerun; status progression is not strictly monotonic
across retries or resumes.

**Response codes:**

| HTTP status | Meaning |
|-------------|---------|
| 200 | Workflow state found; body contains the persisted state plus computed workflow-view fields |
| 404 `command_not_found` | No command with that ID exists |
| 404 `no_workflow` | The command exists but did not run a workflow (e.g. `exec prompt` or `chat`) |
| 404 `workflow_not_started` | An `exec workflow` command is queued or running, but no workflow snapshot exists yet |
| 404 `workflow_never_started` | The workflow command ended before producing any snapshot; stop polling |
| 401 | Missing or invalid API key (same auth middleware as all other endpoints) |

**Polling for live workflow progress:**

Poll this endpoint at your preferred interval to track a running workflow. The state file is updated atomically on every step transition, so polls never observe a partial or corrupted state.

```sh
SERVER=http://localhost:9876
KEY=<your-api-key>

# Poll workflow progress until this command execution stops
while true; do
  RESP=$(curl -s "$SERVER/v1/commands/$CMD/workflow" \
    -H "Authorization: Bearer $KEY")
  PHASE=$(echo "$RESP" | jq -r '.phase // .current_phase')
  echo "Workflow phase: $PHASE"
  COMMAND=$(curl -fsS "$SERVER/v1/commands/$CMD" \
    -H "Authorization: Bearer $KEY") || break
  STATUS=$(echo "$COMMAND" | jq -r '.status')
  case "$STATUS" in
    done|error|cancelled)
      OUTCOME=$(echo "$COMMAND" | jq -r '.result.outcome // .status')
      echo "Command outcome: $OUTCOME"
      break ;;
  esac
  sleep 5
done
```

If the endpoint returns HTTP 404 with `code` `workflow_not_started`, the
workflow state has not been created yet; retry while the command remains queued
or running. `workflow_never_started` means that execution has ended
without a workflow snapshot. A `no_workflow` response means the command did not
run a workflow. If a workflow state was previously returned and later becomes
404, the state was removed; stop polling.

---

## Full example: session lifecycle

```sh
SERVER=https://127.0.0.1:9876
KEY=<your-api-key>   # from the startup banner or --refresh-key
AUTH="Authorization: Bearer $KEY"

# 1. Create a session and wait until its setup is ready
SESSION=$(curl -sk -X POST "$SERVER/v1/sessions" -H "$AUTH" \
  -H 'Content-Type: application/json' \
  -d '{"workdir":"/home/user/my-project"}' | jq -r .session_id)
until [ "$(curl -sk "$SERVER/v1/sessions/$SESSION/status" -H "$AUTH" | jq -r .status)" = ready ]; do
  sleep 2
done

# 2. Submit a command
CMD=$(curl -sk -X POST "$SERVER/v1/commands" -H "$AUTH" \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec workflow","args":["./aspec/workflows/implement-feature.toml"]}' \
  | jq -r .command_id)

# 3. Poll until terminal (queued -> running -> done | error | cancelled)
while true; do
  STATUS=$(curl -sk "$SERVER/v1/commands/$CMD" -H "$AUTH" | jq -r .status)
  echo "Status: $STATUS"
  case "$STATUS" in done|error|cancelled) break ;; esac
  sleep 10
done

# 4. Print the captured output from the JSON event snapshot
curl -sk "$SERVER/v1/commands/$CMD/logs?format=json" -H "$AUTH" \
  | jq -r '.events[] | select(.payload.type == "StdoutLine" or .payload.type == "StderrLine") | .payload.data'

# 5. Close the session
curl -sk -X DELETE "$SERVER/v1/sessions/$SESSION" -H "$AUTH"
```

For a single task without a server, `awman exec prompt "…" --non-interactive` runs locally and exits with the agent's exit code. Going through the API instead records the run durably and makes it visible to other clients.

---

## Storage layout

API runtime files live under `~/.awman/api/` (or `AWMAN_API_ROOT`; see [Create a session](#create-a-session) for the full precedence):

```
~/.awman/api/
  awman.log                        # server log (background mode only)
  awman.pid                        # PID file for the running server
  server.json                      # port and scheme of the running server
  api_key.hash                     # SHA-256 hex digest of the API key (mode 0600)
  tls/cert.pem, tls/key.pem        # self-signed TLS certificate and key
  sessions/
    <session-uuid>/
      setup_state.json             # session setup progress
      <repository-folder>/         # remote sessions only: the clone, deleted on close
      commands/
        <command-uuid>/
          output.log               # combined stdout+stderr (written incrementally)
          events.log               # structured events served by /logs
          output/                  # files the agent wrote to /awman/output
          workflow.state.json      # workflow commands only
          completion.json          # recorded completion, used for restart recovery
          cleanup.json             # pending workflow cleanup; removed after completion
```

The shared SQLite database is stored separately at
`~/.awman/data/awman.db`. On the first start after upgrading, awman migrates
the database from its former API storage location automatically. It retains an
`awman.db.pre-migration` backup of the old database; `awman clean` removes that
backup, but never removes the live database.

`workflow.state.json` is the snapshot served by [Get workflow state](#get-workflow-state). It is written atomically and uses the same JSON structure as the local workflow state in `$GITROOT/.awman/workflows/`.

`awman.db` holds the API session and command records (and squad task and run records): the session and command fields described in this guide, plus labels. It is the authoritative record of activity; the per-command files hold raw output. Neither is deleted when a session is closed (see [Session cleanup](#session-cleanup)).

---

## Server configuration

API server settings live in the global config (`~/.awman/config.json`). All fields are optional.

```json
{
  "workers": 2,
  "api": {
    "workDirs": [
      "/home/user/my-project",
      "/home/user/other-project"
    ],
    "workDirRoots": ["/srv/repos"]
  }
}
```

`workers` (default 2) is the number of queue workers — the maximum number of sessions running a command at once. Edit it in the file; `config set` does not accept it.

### `api.workDirs`

Pre-configure working directories so you don't have to repeat `--workdirs` every time you start the server:

```sh
awman config set --global api.workDirs "/home/user/my-project,/home/user/other-project"
```

Paths from `api.workDirs` and paths from `--workdirs` flags are merged at startup — both sources can be used together. See [Configuration](07-configuration.md#global-config) for the full global config reference.

### `api.workDirRoots`

Pre-configure roots so a client can create sessions on any repository cloned directly beneath them:

```sh
awman config set --global api.workDirRoots "/srv/repos"
```

Merged with `--workdir-root` flags at startup. See [Working directory allowlist](#working-directory-allowlist) for what qualifies as a direct child.

No setting is needed to make API commands non-interactive: the API always runs them that way (see [`flags_applied`](#submit-a-command)). An `api.alwaysNonInteractive` key left in an older config is ignored.

---

## Security

API mode preserves all of awman's container isolation guarantees: every agent runs inside a container on the configured runtime, never directly on the host.

The HTTP server enforces cryptographic API key authentication on every request by default. The plaintext key is shown once at server start and never logged or persisted; only its SHA-256 hash is stored on disk. See [Authentication](#authentication) for the full key lifecycle.

The working directory allowlist is the secondary access control on the server: even a client that presents a valid API key can only create sessions in pre-approved directories.

The server listens on loopback only. If you expose it through a tunnel or proxy, keep authentication and TLS on and restrict who can reach the forwarded port.

---

## Session cleanup

At server startup, awman automatically purges sessions that were closed more than 24 hours ago. Their rows are removed from `~/.awman/data/awman.db` along with all associated command records. The on-disk output logs and workflow snapshots in `~/.awman/api/sessions/<uuid>/` are **not** deleted — they remain for audit purposes, but the API no longer serves a snapshot after its command row is purged. Remove these directories separately when disk retention is no longer needed.

The cleanup runs once, before the server accepts connections, and logs each purged session at `INFO` level. Sessions closed within the last 24 hours are not touched.

---

## Graceful shutdown

On `SIGTERM` or `SIGINT`, the server finishes all in-flight HTTP responses and allows running commands up to 30 seconds to complete before force-terminating them. Both shutdown start and completion are logged. The 30-second grace period applies whether the server was stopped by `awman api kill` or by sending the signal directly.

---

## API edge cases

Cases not covered in the sections above:

| Situation | Behaviour |
|-----------|-----------|
| `--workdirs` path does not exist at startup | Startup refused; create the directory first |
| `--workdir-root` path does not exist at startup | Warning logged; the root is checked again on each request |
| `--workdir-root` is itself inside a Git repository | Startup refused (exit 2); use `--workdirs` for a single repository |
| `api_key.hash` missing at startup | Treated as a first start: a new key is generated and printed |
| `--dangerously-skip-auth` with a hash file on disk | Auth disabled for this run only; the hash file is untouched |
| `exec workflow` with `{{work_item_content}}` and no `--work-item` | Warning; placeholder left unexpanded; the workflow continues |
| Bare `"subcommand": "exec"` (action in `args`) | HTTP 400 `invalid_request`; use `exec prompt` or `exec workflow` as the `subcommand` |
| Session closed between listing it and submitting a command | HTTP 404 `session_not_found` |
| `GET /v1/sessions?status=active` with no active sessions | HTTP 200 `{"sessions":[]}` |
| `GET /v1/commands/:id/workflow` for a paused workflow | HTTP 200; keep polling, since the workflow can resume |

---

# Remote mode

`awman remote` is a command-line client for an awman API server. It creates and closes sessions, submits `exec prompt` and `exec workflow` commands, and streams their output to your terminal. It runs from the CLI or the TUI command box, and takes the same flags in both.

---

## Overview

One machine runs `awman api start` (the _remote host_); developers or pipelines run `awman remote` (the _client_). The remote host manages all sessions and containers.

```
Client                                           Remote host
──────                                           ───────────
awman remote exec workflow my.toml -f ──────────► POST /v1/commands
                                                 ◄── SSE stream: log output
                                                 ◄── `event: done` when the command ends
```

| Command | What it does |
|---------|-------------|
| `awman remote session start` | Create a session on the remote host and wait until it is ready |
| `awman remote exec workflow <file>` | Submit a workflow to a session |
| `awman remote exec prompt <prompt>` | Send a one-shot prompt to a session |
| `awman remote session kill <session-id>` | Close a session |

The API server listens on loopback only, so a client on another machine reaches it through an SSH tunnel or a proxy; see [Reaching the server from another machine](#reaching-the-server-from-another-machine).

---

## Connecting to a remote host

Every `remote` subcommand resolves the server address in this order:

| Priority | Source |
|----------|--------|
| 1 | `--remote-addr <URL>` flag |
| 2 | `AWMAN_REMOTE_ADDR` environment variable |
| 3 | `remote.defaultAddr` in `~/.awman/config.json` |

With none set, the command fails with `remote target address is missing or invalid; pass --remote-addr or set defaultAddr in config`. Set a default once:

```sh
awman config set --global remote.defaultAddr https://127.0.0.1:9876
```

---

## API key authentication

The API key is resolved in this order:

| Priority | Source |
|----------|--------|
| 1 | `--api-key <KEY>` flag |
| 2 | `AWMAN_API_KEY` environment variable |
| 3 | `remote.defaultAPIKey` in `~/.awman/config.json` — **only when the target address matches `remote.defaultAddr`** |

The address match (trailing slashes ignored) keeps a stored key from being sent to a different server: when `--remote-addr` or `AWMAN_REMOTE_ADDR` points elsewhere, the config key is ignored and a server that requires auth answers HTTP 401.

```sh
awman config set --global remote.defaultAPIKey <your-api-key>
```

For CI, use environment variables:

```sh
export AWMAN_REMOTE_ADDR=https://127.0.0.1:9876
export AWMAN_API_KEY=<your-api-key>
```

`AWMAN_API_KEY` is visible to other processes of the same user (for example `/proc/<pid>/environ` on Linux). In sensitive contexts prefer the config file with restricted permissions (`chmod 600 ~/.awman/config.json`).

---

## `awman remote session start`

Creates a session on the remote host, prints setup progress, and returns once the session is `ready` (or fails if setup fails).

```sh
# Local session: a directory on the remote host that is on its allowlist
awman remote session start --workdir /home/user/my-project

# Remote session: the server clones the repository
awman remote session start --type remote \
  --repo-url https://github.com/org/my-project --branch main
```

| Flag | Description |
|------|-------------|
| `--type local\|remote` | Session type (default `local`) |
| `--workdir <path>` | Required for `local`: an absolute path on the remote host, allowed by its [working directory allowlist](#working-directory-allowlist) |
| `--repo-url <url>` | Required for `remote` |
| `--branch <name>` | Optional for `remote` |
| `--remote-addr`, `--api-key` | See above |

On success awman prints `Session created: <id>`, the setup stages, the same ready summary box `awman ready` draws, and finally on stdout:

```
Session a1b2c3d4-e5f6-7890-abcd-ef1234567890 created via https://127.0.0.1:9876 [ready].
```

To capture the ID in a script: `SESSION=$(awman remote session start --workdir … | awk '/ created via / {print $2}')`. For `base_ref`, `merge_refs` or `push`, create the session through the [HTTP API](#create-a-session).

---

## `awman remote exec workflow`

Submits a workflow file to a ready session. The path is resolved on the remote host, relative to the session's working directory.

```sh
# Submit; print the command ID and return
awman remote exec workflow aspec/workflows/implement-feature.toml --session abc123

# Stream output until the command finishes
awman remote exec workflow aspec/workflows/implement-feature.toml --work-item 0042 --session abc123 -f
```

| Flag | Short | Description |
|------|-------|-------------|
| `--session <ID>` | | Session ID on the remote host. Falls back to `AWMAN_REMOTE_SESSION` |
| `--follow` | `-f` | Stream output until the command completes |
| `--remote-addr <URL>` | | Remote host address |
| `--api-key <KEY>` | | API key |
Every other flag is an [`exec workflow`](#awman-exec-workflow-path--awman-exec-wf-path) flag, and every one you pass is sent to the server, which runs `exec workflow` with it: `--work-item`, `--issue`, `--agent`, `--model`, `--yolo`, `--auto`, `--plan`, `--overlay`, `--agent-flag`, `--dynamic` and `--leader`, `--max-concurrent`, the `--on-…` choices, the `--prerequisite-…` grants and `--check-prerequisites`. Paths in them (`--overlay`, the workflow file) are paths on the remote host, and `--agent-flag` replaces the remote host's `agentFlags` entry for the run. The server then applies its API defaults (non-interactive, and `--yolo` unless you passed `--plan` or `--auto`); see [Submit a command](#submit-a-command).

Two `exec workflow` flags are not available, because they describe the local run: `--worktree` (the server decides; its API default `--yolo` implies one) and `--json` (the server reports the result; read it with `GET /v1/commands/<id>/status`). Passing either is an unknown-flag error.

With `--dynamic`, omit the workflow path; the server's leader agent designs the workflow for `--work-item` or `--issue`:

```sh
awman remote exec workflow --dynamic --work-item 0042 --session abc123 -f
```

Without `--session` or `AWMAN_REMOTE_SESSION`, the command fails with ``no remote session id; pass --session <id> or run `awman remote session start` first``.

### Live log streaming (`--follow`)

Without `--follow`, awman prints `Command submitted: <id>` and returns. With `--follow`, it streams the command's events: stdout and stderr lines go to the matching stream (prefixed with `[step]` once more than one workflow step has produced output), and status, step and phase transitions are printed as `[...]` lines. When the command ends it prints a final line on stdout:

```
Command e5f6a7b8-…: workflow (session abc123) via https://127.0.0.1:9876 [done].
```

- A command that already finished is replayed in full, then the final line is printed.
- If the connection drops, awman reconnects once after 2 seconds.
- **Ctrl-C** stops following; the command keeps running on the server.
- With `--follow`, awman exits with the remote command's result: `0` when it finished `done`, the command's own exit code when that is non-zero, and `1` when it failed or was cancelled without one, or when following stopped before it finished (Ctrl-C, a lost connection). Without `--follow`, a successful submit exits `0`.

---

## `awman remote exec prompt`

Sends a one-shot prompt to a ready session. Session resolution, `--follow` and its exit code work as for `remote exec workflow`. Every [`exec prompt`](#awman-exec-prompt-prompt) flag you pass (`--agent`, `--model`, `--issue`, `--yolo`, `--plan`, `--overlay`, `--agent-flag` and the rest) is sent to the server. Put flags before the prompt: everything after the prompt's first word is part of the prompt.

```sh
awman remote exec prompt "Fix the failing tests" --session abc123 --follow
```

---

## `awman remote session kill`

Closes a session on the remote host. A running command is allowed to finish; see [Close a session](#close-a-session).

```sh
awman remote session kill a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

On success awman prints `Session <id> killed.` A session the server does not know is treated as already closed.

---

## Remote client configuration

Remote settings live under `remote` in the global config (`~/.awman/config.json`):

```json
{
  "remote": {
    "defaultAddr": "https://127.0.0.1:9876",
    "defaultAPIKey": "a3f8b2c1...64-char-hex..."
  }
}
```

| Key | Meaning |
|-----|---------|
| `remote.defaultAddr` | Default server address. Overridden by `--remote-addr` or `AWMAN_REMOTE_ADDR` |
| `remote.defaultAPIKey` | Key sent only to `remote.defaultAddr`. Overridden by `--api-key` or `AWMAN_API_KEY` |

---

## Full example: CI pipeline

```sh
export AWMAN_REMOTE_ADDR=https://127.0.0.1:9876   # tunnelled to the build host
export AWMAN_API_KEY=<your-api-key>

SESSION=$(awman remote session start --type remote \
  --repo-url https://github.com/org/my-project --branch main \
  | awk '/ created via / {print $2}')
awman remote exec workflow aspec/workflows/implement-feature.toml \
  --work-item 0042 --session "$SESSION" --follow | tee workflow.log
awman remote session kill "$SESSION"
```

For a pre-provisioned, long-lived session, set `AWMAN_REMOTE_SESSION` instead of passing `--session`.

---

## Remote edge cases

| Situation | Behaviour |
|-----------|-----------|
| Remote host unreachable | Fails after a 10-second connect timeout |
| Session not ready yet | HTTP 409 `session_not_ready`; wait for `remote session start` to report ready |
| Session closed or unknown on `remote exec` | HTTP 404 `session_not_found` |
| No API key and the server requires one | HTTP 401 `unauthorized` |
| `remote.defaultAPIKey` set but the target address differs from `remote.defaultAddr` | Config key ignored; the server answers 401 if auth is required |

---

[← Overlays](08-overlays.md) · [Next: GitHub Integration →](10-github-integration.md)
