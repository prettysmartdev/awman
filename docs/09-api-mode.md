# API Mode

API mode exposes awman's session and subcommand execution over HTTP. Start a persistent server with `awman api start`, then drive sessions and subcommands from scripts, CI pipelines, or any HTTP client — no interactive terminal or TUI required.

A **session** in API mode is conceptually identical to a TUI tab: a named, isolated workspace bound to a working directory. Subcommands dispatched to a session (`exec workflow`, `chat`, etc.) execute exactly as they would in a TUI tab — inside a Docker container, with all the same security and isolation guarantees.

All operations, inputs, and outputs are recorded durably in `~/.awman/api/` for auditability.

---

## When to use API mode

API mode is useful for:

- CI pipelines that trigger `exec workflow` or `exec prompt` runs and poll for results
- Scripts or tooling that execute workflows and retrieve output programmatically
- Remote integrations where the awman server runs on one machine and clients run elsewhere
- Audit-heavy environments where a complete durable record of every agent action is required
- One-shot agent invocations from scripts using `awman exec prompt` or `awman exec workflow`

For single interactive sessions, use `awman chat` instead.

---

## Quickstart — copy-pastable examples

These examples assume a server running on `127.0.0.1:9876`. The server speaks HTTPS by default with a self-signed cert, so curl needs `-k` (or `--cacert ~/.awman/api/tls/cert.pem`) to skip verification. Pass `--dangerously-skip-tls` at server start if you'd rather use plain HTTP.

```sh
# 0. Start the server (one-time; stores its api_key.hash under ~/.awman/api).
#    The plaintext key is shown ONCE in the startup banner; copy it now.
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
  -d "{\"type\":\"local\",\"workdir\":\"$HOME/my-project\"}" | jq -r .session_id)

# 2. Dispatch a one-shot prompt; the response carries the command_id
CMD=$(curl -sk -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H "x-awman-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["prompt","Summarise recent changes","--non-interactive"]}' \
  | jq -r .command_id)

# 3. Stream the output until the command finishes
curl -Nsk "$SERVER/v1/commands/$CMD/logs/stream" \
  -H "Authorization: Bearer $KEY" \
  | while IFS= read -r line; do
      case "$line" in
        "data: [awman:done]") break ;;
        data:\ *)              printf '%s\n' "${line#data: }" ;;
      esac
    done

# 4. Close the session
curl -sk -X DELETE "$SERVER/v1/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY"
```

**Using `awman` itself** (no manual session handling — `awman remote` handles it):

```sh
# One-time setup so subsequent commands don't need flags
awman config set --global remote.defaultAddr "$SERVER"
awman config set --global remote.defaultAPIKey "$KEY"

# Start a session bound to the local directory and capture its ID
SESSION=$(awman remote session start "$HOME/my-project" \
  | awk '/Session started:/ {print $NF}')

# Dispatch the same prompt and stream its output live
awman remote run exec prompt "Summarise recent changes" --non-interactive \
  --session "$SESSION" --follow

# Close the session
awman remote session kill "$SESSION"
```

### Remote session (server clones a Git repo)

A remote session asks the server to clone a Git repo into an ephemeral directory under `~/.awman/sessions/<session-id>/`. The clone is deleted when the session is closed.

**Using `curl`:**

```sh
# 1. Create a remote session — the server does the clone for you
SESSION=$(curl -sk -X POST "$SERVER/v1/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "type": "remote",
    "repo_url": "https://github.com/org/my-project",
    "branch": "main"
  }' | jq -r .session_id)

# 2. Run a workflow against the clone
CMD=$(curl -sk -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H "x-awman-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["workflow","aspec/workflows/implement-feature.toml","--work-item","0053"]}' \
  | jq -r .command_id)

# 3. Poll status until terminal
while true; do
  STATUS=$(curl -sk "$SERVER/v1/commands/$CMD" \
    -H "Authorization: Bearer $KEY" | jq -r .status)
  echo "status=$STATUS"
  [ "$STATUS" = "done" ] || [ "$STATUS" = "error" ] && break
  sleep 5
done

# 4. Closing deletes the on-server clone
curl -sk -X DELETE "$SERVER/v1/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY"
```

**Using `awman` itself:** the `awman remote session start` command currently expects a path that already exists on the server, so for remote (clone-on-server) sessions create the session via curl as shown above, then drive it with `awman remote run --session <id>`:

```sh
# Drive the just-created remote-clone session with awman
awman remote run exec workflow aspec/workflows/implement-feature.toml \
  --work-item 0053 \
  --session "$SESSION" --follow
```

---

## One-shot scripted execution (`exec`)

The `exec` subcommand group provides two commands for running agent tasks non-interactively from scripts, CI pipelines, or the API server — without a persistent session or TUI.

### `awman exec prompt <prompt>`

Launches an agent container with a pre-supplied prompt. Behaves identically to `awman chat`, except the initial prompt is baked into the launch arguments rather than requiring a live terminal session.

```sh
# Run a single task and exit
awman exec prompt "Fix the failing tests in src/api"

# Non-interactive: agent executes and exits; output goes to stdout
awman exec prompt "Summarise recent changes" --non-interactive

# Use a specific agent and model
awman exec prompt "Refactor the auth module" --agent codex --model gpt-4o

# Full autonomous run
awman exec prompt "Implement caching for the API layer" --yolo --non-interactive
```

The prompt must be non-empty. Passing an empty string exits immediately with:

```
error: prompt cannot be empty
```

**Flags accepted by `exec prompt`:**

| Flag | Description |
|------|-------------|
| `--non-interactive` / `-n` | Run in print/batch mode — agent executes and exits |
| `--plan` | Read-only analysis mode — agent cannot modify files |
| `--allow-docker` | Mount host Docker socket into the container |
| `--mount-ssh` | Mount `~/.ssh` read-only into the container |
| `--auto` | Auto-approve file edits; prompt before shell commands |
| `--yolo` | Fully autonomous mode — skip all permission prompts |
| `--agent=<name>` | Override the agent for this run |
| `--model=<NAME>` | Override the model for this run |

All flags behave identically to their `chat` counterparts. See [Agent Sessions](02-agent-sessions.md#flags-common-to-chat-and-other-commands).

---

### `awman exec workflow <path>` / `awman exec wf <path>`

Runs a workflow file. The work item is optional — when provided, it's used for template variable substitution within the workflow.

```sh
# Run a workflow without a work item
awman exec workflow ./aspec/workflows/implement-feature.md

# Alias: exec wf
awman exec wf ./aspec/workflows/implement-feature.md

# Optionally associate a work item for template variable substitution
awman exec workflow ./aspec/workflows/implement-feature.md --work-item 0053

# Non-interactive workflow run
awman exec workflow ./aspec/workflows/review.md --non-interactive
```

`exec workflow` and `exec wf` are identical — `wf` is a short alias.

**Work item template variables:** When no `--work-item` is given, prompt templates that use `{{work_item_number}}`, `{{work_item_content}}`, or `{{work_item_section:[Name]}}` are left unexpanded with a warning:

```
warning: workflow uses {{work_item_content}} but no --work-item was provided; placeholder left unexpanded
```

When `--work-item <N>` is provided, awman resolves the work item file from the configured work items directory and substitutes all template variables.

**Workflow state files:** When no work item is given, the state file is keyed by the workflow file's name and content hash:

```
~/.awman/api/<workflow-name>-<content-hash8>.state.json
```

When a work item is given, the state file is saved to:

```
$GITROOT/.awman/workflows/<repo-hash8>-<work-item>-<workflow-name>.json
```

**Flags accepted by `exec workflow`:**

| Flag | Description |
|------|-------------|
| `--work-item <N>` / `-w <N>` | Work item number; enables template variable substitution |
| `--non-interactive` / `-n` | Run each step's agent in print/batch mode |
| `--plan` | Read-only mode for all steps |
| `--allow-docker` | Mount host Docker socket into each step's container |
| `--worktree` | Run all steps in an isolated Git worktree |
| `--mount-ssh` | Mount `~/.ssh` read-only into each step's container |
| `--auto` | Auto-approve file edits; prompt before shell commands |
| `--yolo` | Fully autonomous mode; implies `--worktree`; auto-advances stuck steps |
| `--agent=<name>` | Default agent for steps that do not specify an `Agent:` field |
| `--model=<NAME>` | Default model for steps that do not specify a `Model:` field |

All workflow flags are described in [Workflows](04-workflows.md#flags).

---

## Starting the server

### Foreground

```sh
awman api start --port 9876 --workdirs /path/to/repo
```

The server starts on the specified port (default `9876`) and accepts requests for the life of the process. The first start auto-generates an API key and prints it in a banner — copy it then; it isn't shown again. Subsequent starts reuse the same hash silently. By default the server speaks HTTPS on a self-signed cert; pass `--dangerously-skip-tls` to fall back to plain HTTP for trusted-local use. Logs are emitted to stderr (color-coded when stderr is a TTY); press `Ctrl+C` to stop.

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

`--workdirs` accepts one or more absolute paths (repeat the flag for multiple values). Only working directories on the allowlist can be used to create sessions — requests with any other path are rejected with HTTP 403. See [Working directory allowlist](#working-directory-allowlist).

**Start flags related to authentication and transport:**

| Flag | Description |
|------|-------------|
| `--refresh-key` | Generate a new API key, write its hash to disk, and print the plaintext key. The old key is invalidated immediately. The command exits after printing — it does not start the server. |
| `--dangerously-skip-auth` | Disable authentication for this process lifetime only. The `api_key.hash` file is left untouched; the next normal start re-enables auth using the stored hash. |
| `--dangerously-skip-tls` | Serve plain HTTP on the loopback interface instead of HTTPS. Intended for tests and trusted-local environments only; the server logs a loud warning at startup. Clients must connect with `http://` (not `https://`). |

### Background

```sh
awman api start --background --port 9876 --workdirs /path/to/repo
```

`--background` daemonizes the server using the OS process manager:

| Platform | Mechanism |
|----------|-----------|
| Linux (systemd available) | `systemd-run --user` writes a transient unit |
| macOS (launchd) | Writes `~/Library/LaunchAgents/io.awman.api.plist` and calls `launchctl load` |
| Fallback (no systemd/launchd) | Double-fork; PID written directly |

The PID is stored at `~/.awman/api/awman.pid`. Logs go to `~/.awman/api/awman.log`.

If a server is already running (detected via `awman.pid` and a live process check), `start` prints an error and exits with a non-zero code rather than silently competing for the port.

If `bind()` fails because the port is already in use, the error message includes the port number and the PID holding it (when discoverable):

```
error: port 9876 is already in use (PID 41290)
```

---

## Server lifecycle commands

### Status

```sh
awman api status
```

Prints whether the server is running, its PID, port, active session count, and uptime:

```
Status:          running
PID:             81234
Port:            9876
Active sessions: 2
Uptime:          3h 14m
```

If the server is not running:

```
Status:  not running
```

### Logs

```sh
awman api logs
```

Streams `~/.awman/api/awman.log` to stdout in real time (equivalent to `tail -f`). Only available when the server was started with `--background`. Press `Ctrl+C` to stop streaming.

If no log file exists:

```
error: no log file found at ~/.awman/api/awman.log
       start the server with --background to enable file logging
```

#### Session-setup verbosity

While a session is being prepared (`cloning_repository`, `setting_up_branch`, `running_ready`), the server writes the full ready output — including container build lines, git command output, and state transitions — to the API log file. Each line is prefixed with the first 8 characters of the session id so you can grep one session out of a busy log:

```sh
grep '\[a1b2c3d4\]' ~/.awman/api/awman.log
```

To silence this verbose stream, set `AWMAN_API_VERBOSE_SETUP=0` before starting the server. The lines are then emitted at `debug` level and dropped by the default `info` filter; they remain available via `RUST_LOG=debug`.

### Kill

```sh
awman api kill
```

Sends `SIGTERM` to the background server process, allows in-flight requests to drain (up to the graceful shutdown period), and removes the PID file. On macOS, also unloads the launchd plist.

If the server is not running:

```
info: server is not running (no PID file found)
```

---

## Authentication

The API server uses cryptographic API key authentication. Every HTTP request must include the API key; unauthenticated requests are rejected with HTTP 401 before reaching any handler.

### First start — key generation

On the first start (when no `api_key.hash` file exists in `~/.awman/api/`), awman automatically generates a cryptographically random 32-byte key, stores only its SHA-256 hash on disk (never the plaintext), and prints the key to stdout once:

```
╔═══════════════════════════════════════════════════════════════════╗
║  awman API key (store this — it will not be shown again)          ║
║  a3f8b2c1...64-character-hex-key...d7e9f0a1                       ║
╚═══════════════════════════════════════════════════════════════════╝
```

**This is the only time the plaintext key appears.** Store it immediately. Key generation happens before the log file is opened, so the key cannot appear in `awman.log` under any circumstances.

### Subsequent starts

On subsequent starts (when `~/.awman/api/api_key.hash` already exists), the server loads the stored hash silently and starts normally — no banner is printed. The same key continues to work without any client-side changes.

### Rotating the key — `--refresh-key`

To invalidate the current key and generate a new one:

```sh
awman api start --refresh-key --port 9876 --workdirs /repo
```

A new key is generated and its hash replaces the file on disk. The new plaintext key is printed to stdout using the same banner format. All clients using the old key will immediately receive HTTP 401 and must be updated.

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

When using `awman remote` subcommands, the key is resolved and injected automatically — see [Remote Mode: API key](10-remote-mode.md#api-key-authentication).

**Error responses from the middleware:**

| Situation | HTTP status | Response body |
|-----------|-------------|---------------|
| No `Authorization` header | 401 | `{"error": "API key required. Pass the key via the Authorization header (e.g. Authorization: Bearer <key>)."}` |
| Wrong key | 401 | `{"error": "Invalid API key."}` |

Hash comparisons use constant-time comparison (`ring::constant_time::verify_slices_are_equal`) to prevent timing attacks.

### Key storage

The key hash is stored at `~/.awman/api/api_key.hash` with mode `0o600` (owner read/write only on Unix). Only the SHA-256 hex digest is written — the plaintext key is never persisted anywhere.

---

## Working directory allowlist

The server maintains a strict allowlist of working directories. Any session creation request that specifies a path not on the allowlist is rejected with HTTP 403.

**At startup**, the allowlist is populated from two sources:

1. `--workdirs` flags passed to `awman api start`
2. `api.workDirs` in the global config (`~/.awman/config.json`)

Both sources are merged. Every path is resolved to its canonical form (symlinks resolved, trailing slashes stripped) via `std::fs::canonicalize`. If a listed path does not exist at startup, a warning is logged but the server still starts — the path stays on the allowlist in case the directory is created later.

To see the current allowlist over HTTP:

```sh
curl http://localhost:9876/v1/workdirs
```

```json
{
  "workdirs": [
    "/home/user/my-project",
    "/home/user/other-project"
  ]
}
```

---

## Job Queue

The API server uses a per-session FIFO queue to serialize command execution. When you submit a command via `POST /v1/commands`, it is enqueued immediately and returns with a `command_id`. A pool of worker tasks processes the queue: each worker claims one queued command at a time and executes it, then claims the next one.

**Key properties:**

- **One command per session at a time**: Within a single session, only one command runs at any moment. Commands are processed in strict FIFO order (ordered by submission time). This ensures workflows don't interfere with each other.

- **Concurrent execution across sessions**: With N worker tasks and M sessions with queued commands, up to min(N, M) commands can run *concurrently* — one per session. For example, with 2 workers and 3 sessions, 2 sessions can execute in parallel while the 3rd waits.

- **Never blocks submission**: Unlike the old behavior, submitting a command never blocks or returns "session busy". The command is enqueued and the request returns immediately with HTTP 202 (if successful) or 409 (if the session is closing). The caller polls `GET /v1/commands/{id}/status` or `GET /v1/sessions/{id}/queue` to check progress.

**Monitoring queue status:**

Use `GET /v1/sessions/{id}/queue` to inspect the queue depth, which command is currently running, and recently completed commands. This is useful for:

- Checking how long commands will take to run (based on queue depth)
- Diagnosing stalls or stuck commands
- Capacity planning (how many workers to run)

**Configuration:**

The number of worker tasks is configurable via the `workers` global config option. The default is 2 workers. See [Configuration](#configuration) below.

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
| `GET` | `/v1/workdirs` | List the server's allowlisted working directories |
| `POST` | `/v1/sessions` | Create a new session |
| `GET` | `/v1/sessions` | List sessions; accepts optional `?status=active` filter |
| `GET` | `/v1/sessions/:id` | Get session detail |
| `GET` | `/v1/sessions/:id/queue` | Get queue status for a session (queued, running, recent completed) |
| `DELETE` | `/v1/sessions/:id` | Close a session |
| `POST` | `/v1/commands` | Submit a subcommand to a session (enqueued) |
| `GET` | `/v1/commands/:id` | Get command status and metadata |
| `GET` | `/v1/commands/:id/logs` | Get captured command output (snapshot) |
| `GET` | `/v1/commands/:id/logs/stream` | Stream live command output via Server-Sent Events |
| `GET` | `/v1/workflows/:id` | Get the workflow state for a command |
| `GET` | `/v1/status` | Server health (uptime, active sessions, running commands) |

---

### Sessions

#### Create a session

There are two types of sessions:

**Local session** — bound to a working directory on the server's filesystem:

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"type":"local","workdir":"/home/user/my-project"}'
```

**Remote session** — clones a git repository on the server and operates within the clone:

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"type":"remote","repo_url":"https://github.com/org/repo","branch":"main"}'
```

Both types return immediately with a session UUID:

```json
{ "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890" }
```

**Local sessions** bind to an existing directory on the server. The directory must be in the allowlist (configured via `api.workDirs` in global config or `--workdirs` at startup). Workflows run directly in that directory.

**Remote sessions** clone a git repository into an isolated directory on the server (`~/.awman/sessions/{session_id}/repo/`). Workflows run inside the clone. When the session is closed, the cloned directory is deleted, leaving no artifacts on the server. This is useful for running ephemeral CI jobs where you want to operate on a remote repo without setting up a local copy.

#### End-to-end example: Remote session workflow with setup and teardown

This example demonstrates using a remote session to run an automated feature implementation workflow with setup and teardown steps. The repo is cloned by awman at session creation; setup installs dependencies; the main workflow steps implement the feature; teardown runs tests and creates a pull request.

**1. Create a remote session:**

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{
    "type": "remote",
    "repo_url": "https://github.com/org/my-project",
    "branch": "main"
  }'
```

Response:
```json
{ "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890" }
```

At this point, the repository has been cloned on the server and the main branch has been checked out. The session's working directory contains a fresh clone.

**2. Create a workflow file with setup and teardown (saved to your repo as `aspec/workflows/implement-feature.toml`):**

```toml
name = "implement-feature"
teardown_on_failure = true

[[setup]]
type = "checkout_create_branch"
branch = "feature/auto-implementation"
base = "main"

[[setup]]
type = "run_shell"
command = "npm install"

[[setup]]
type = "run_shell"
command = "npm run build"

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
add_all = true

[[teardown]]
type = "push_branch"
remote = "origin"
branch = "feature/auto-implementation"

[[teardown]]
type = "create_pull_request"
title = "feat: auto-implemented feature"
body = "This PR was automatically generated by an awman workflow."
base = "main"
```

**3. Submit the workflow to the remote session:**

```sh
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'x-awman-session: a1b2c3d4-e5f6-7890-abcd-ef1234567890' \
  -H 'Content-Type: application/json' \
  -d '{
    "subcommand": "exec",
    "args": ["workflow", "aspec/workflows/implement-feature.toml", "--work-item", "0053"]
  }'
```

Response:
```json
{ "command_id": "e5f6a7b8-c9d0-e1f2-3456-7890abcdef01" }
```

**4. What happens next:**

The workflow executes in three phases:

1. **Setup phase:** Inside a background container with the cloned repo mounted:
   - Check out a new feature branch from main
   - Run `npm install` to install dependencies
   - Run `npm run build` to build the project

2. **Main phase:** Run the implement and review steps (agent sessions as usual)

3. **Teardown phase:** After main steps complete, inside a new background container:
   - Run the test suite (`npm test`)
   - Commit all changes with a descriptive message
   - Push the feature branch to the remote (`origin`)
   - Create a pull request via the GitHub CLI

If any step fails (including setup), all subsequent steps are skipped. Because `teardown_on_failure = true`, the teardown phase still runs even if the main workflow fails — useful for cleanup or test reports.

**5. Monitor progress:**

```sh
# Check command status
curl -s http://localhost:9876/v1/commands/e5f6a7b8-c9d0-e1f2-3456-7890abcdef01 \
  -H 'Authorization: Bearer <api-key>'

# Stream output live
curl -s http://localhost:9876/v1/commands/e5f6a7b8-c9d0-e1f2-3456-7890abcdef01/logs/stream \
  -H 'Authorization: Bearer <api-key>'
```

**Key observations:**

- The repository provisioning (`clone`, `branch checkout`) is handled automatically by awman's `GitEngine` when the session is created — the setup phase does not need to clone the primary repo.
- Setup and teardown steps run inside isolated containers, ensuring no side effects on the host.
- All environment variables configured for the project (overlays, env vars) are available in setup and teardown containers.
- If the workflow is interrupted and resumed, the entire setup phase re-runs (steps should be idempotent — e.g., use `npm install` which is safe to re-run).
- The `create_pull_request` step requires the `gh` (GitHub CLI) tool to be available in the base container image. Ensure your `Dockerfile.dev` includes it.
- When the session is closed, the cloned directory and all generated artifacts are deleted from the server.

Error responses:

| Situation | HTTP status |
|-----------|-------------|
| `workdir` not in allowlist (local) | 403 — includes `allowed_workdirs` list |
| Invalid `type` field | 400 |
| Missing required fields | 400 |
| Network error cloning repo (remote) | 500 |

#### List sessions

```sh
# All sessions (active and closed)
curl -s http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>'

# Active sessions only
curl -s "http://localhost:9876/v1/sessions?status=active" \
  -H 'Authorization: Bearer <api-key>'
```

The optional `status` query parameter filters by session status. Accepted values: `active`, `closed`. Without the parameter, all sessions are returned.

```json
{
  "sessions": [
    {
      "id": "a1b2c3d4-...",
      "workdir": "/home/user/my-project",
      "status": "active",
      "created_at": "2026-04-20T12:00:00Z",
      "closed_at": null
    },
    {
      "id": "b2c3d4e5-...",
      "workdir": "/home/user/my-project",
      "status": "closed",
      "created_at": "2026-04-19T09:00:00Z",
      "closed_at": "2026-04-19T09:47:13Z"
    }
  ]
}
```

#### Get session detail

```sh
curl -s http://localhost:9876/v1/sessions/<session-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the session record. Returns HTTP 404 if the ID does not exist.

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
  -H 'x-awman-session: <session-id>' \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"chat"}'
```

Dispatches a subcommand to the session identified by the `x-awman-session` header. Valid values for `subcommand`: `chat`, `ready`, `exec`, `remote`.

For `exec`, the `args` array starts with the exec action (`prompt` or `workflow`/`wf`), followed by any further arguments:

```sh
# exec prompt via API
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'x-awman-session: <session-id>' \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["prompt","Fix the failing tests","--non-interactive"]}'

# exec workflow via API
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'x-awman-session: <session-id>' \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["workflow","./aspec/workflows/implement-feature.md","--work-item","0053"]}'
```

**Workflow files:** For `exec workflow`, the workflow file path is specified as a positional argument relative to the session's working directory (same format as the CLI). The file must exist within the session's working directory and be a valid TOML or YAML workflow definition. Workflow files are never passed as inline JSON — always use file paths.

Returns immediately with a command UUID — the command is **enqueued** and execution is asynchronous:

```json
{ "command_id": "e5f6a7b8-..." }
```

The command begins executing as soon as a worker claims it from the queue. **At most one command per session runs at any given time** — commands within a session are processed sequentially in FIFO order. If a command is already running, subsequent commands are queued and wait their turn (they do not block the API request).

Error responses:

| Situation | HTTP status | `error` field |
|-----------|-------------|---------------|
| Session not found or closed | 404 | `"session not found"` (includes session UUID) |
| Session is closing (graceful shutdown) | 409 | `"session is closing"` |
| Unknown subcommand | 400 | `"unknown subcommand"` (includes list of valid subcommands) |
| `x-awman-session` header missing | 400 | `"missing x-awman-session header"` |

#### Get command status

```sh
curl -s http://localhost:9876/v1/commands/<command-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current status and metadata for a command:

```json
{
  "id": "e5f6a7b8-...",
  "session_id": "a1b2c3d4-...",
  "subcommand": "exec",
  "args": ["workflow", "deploy.toml"],
  "status": "queued",
  "queued_at": "2026-04-20T12:00:30Z",
  "queue_position": 2,
  "exit_code": null,
  "started_at": null,
  "finished_at": null,
  "log_path": "~/.awman/api/sessions/a1b2c3d4-.../commands/e5f6a7b8-.../output.log"
}
```

| `status` value | Meaning |
|----------------|---------|
| `queued` | Enqueued; waiting for a worker to claim it |
| `running` | Claimed by a worker; container is executing |
| `done` | Completed with exit code 0 |
| `error` | Completed with a non-zero exit code |
| `cancelled` | Cancelled before execution (e.g. session kill) |

**Queue-related response fields** (present when `status = 'queued'`):

| Field | Description |
|-------|-------------|
| `queued_at` | ISO 8601 timestamp when the command was enqueued |
| `queue_position` | 0-indexed position in the session's queue (0 = next to run). `null` when the command is running, done, or cancelled |
| `worker_id` | UUID of the worker task that claimed this command. Only present when `status = 'running'` |

**Result field** (present when `status = 'done'` or `'error'`):

```json
{
  "exit_code": 0,
  "error": null
}
```

#### Get command logs

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/logs \
  -H 'Authorization: Bearer <api-key>'
```

Returns the captured output for a command. Stdout and stderr are combined into a single stream in the order they were written. For a running command, returns whatever has been written so far. For a completed command, returns the full output.

```json
{
  "output": "Implementing work item 0057...\n✓ Tests pass\n"
}
```

Output is written incrementally as the subprocess produces it — not buffered in memory.

#### Stream command logs (live)

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/logs/stream \
  -H 'Authorization: Bearer <api-key>'
```

Opens a persistent HTTP response using [Server-Sent Events (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events). The server replays any output already written, then tails the log file and sends new lines as they arrive. When the command completes, the server sends a `[awman:done]` sentinel event and closes the response.

**SSE event format:**

```
data: <line of log output>

data: <another line>

data: [awman:done]

```

Each event is terminated by a blank line (standard SSE format). The sentinel `[awman:done]` signals that the command has finished — no more output will follow.

**Shell example — stream and print until done:**

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/logs/stream \
  -H 'Authorization: Bearer <api-key>' \
| while IFS= read -r line; do
    case "$line" in
      "data: [awman:done]") echo "--- done ---"; break ;;
      data:\ *)              echo "${line#data: }" ;;
    esac
  done
```

**Behaviour notes:**

- If the command has already completed when you connect, the server replays the full historical log and sends `[awman:done]` immediately — no output is missed.
- If the client disconnects mid-stream, the command continues executing unaffected.
- If the log file does not yet exist (the command is `pending`), the server waits up to 10 s for it to appear before returning HTTP 404.
- The `Content-Type` response header is `text/event-stream`.
- **Read timeout:** `awman remote run --follow` uses a 10-minute read timeout per SSE event. Any output from the server resets the timer, so long-running commands that produce incremental output can stream for hours. If the server is completely silent for 10 minutes, the client disconnects with a timeout message; the command continues running on the server. When using cURL directly with `--no-buffer`, consider adding `--max-time 0` to disable cURL's own timeout for very long-running commands.

`awman remote run --follow` uses this endpoint internally. The cURL form above is equivalent and is useful in scripts where the awman binary is unavailable on the client.

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
    "subcommand": "exec",
    "args": ["workflow", "deploy.toml"],
    "started_at": "2026-04-20T12:05:00Z",
    "worker_id": "worker-789"
  },
  "queued": [
    {
      "command_id": "cmd-457",
      "subcommand": "exec",
      "args": ["workflow", "test.toml"],
      "queued_at": "2026-04-20T12:01:00Z",
      "position": 0
    },
    {
      "command_id": "cmd-458",
      "subcommand": "exec",
      "args": ["prompt", "review the code"],
      "queued_at": "2026-04-20T12:02:00Z",
      "position": 1
    }
  ],
  "recent_completed": [
    {
      "command_id": "cmd-455",
      "subcommand": "exec",
      "args": ["workflow", "setup.toml"],
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
| `recent_completed` | Array of the 10 most recent completed commands (status `done` or `error`), newest first |

This endpoint is useful for monitoring queue depth and diagnosing stalls in multi-command workflows.

---

### Server health

```sh
curl -s http://localhost:9876/v1/status \
  -H 'Authorization: Bearer <api-key>'
```

```json
{
  "uptime_seconds": 11640,
  "active_sessions": 2,
  "running_commands": 1
}
```

---

### Workflow state

When a command runs a workflow (`exec workflow`), the API server writes a `workflow.state.json` file to the per-command directory. This file is updated atomically on every step transition. The `GET /v1/workflows/:command_id` endpoint exposes that state over HTTP.

#### Get workflow state

```sh
curl -s http://localhost:9876/v1/workflows/<command-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current `WorkflowState` for the given command. The structure is identical to the local workflow state format produced by `awman exec workflow` when it writes state to `$GITROOT/.awman/workflows/`.

```json
{
  "steps": [
    { "name": "plan",      "status": "done"    },
    { "name": "implement", "status": "running" },
    { "name": "docs",      "status": "pending" },
    { "name": "review",    "status": "pending" }
  ],
  "status": "running"
}
```

Use the `status` field in the response body to determine completion — do not rely solely on the HTTP status code.

| `status` value | Meaning |
|----------------|---------|
| `running` | At least one step is actively executing |
| `paused` | A step is waiting for user confirmation to advance |
| `complete` | All steps finished successfully |
| `error` | At least one step failed |

**Response codes:**

| HTTP status | Meaning |
|-------------|---------|
| 200 | Workflow state found; body contains the full `WorkflowState` JSON |
| 404 `{"error": "command not found"}` | No command with that ID exists |
| 404 `{"error": "no workflow for this command"}` | The command exists but did not run a workflow (e.g. `exec prompt` or `chat`) |
| 401 | Missing or invalid API key (same auth middleware as all other endpoints) |

**Polling for live workflow progress:**

Poll this endpoint at your preferred interval to track a running workflow. The state file is updated atomically on every step transition, so polls never observe a partial or corrupted state.

```sh
SERVER=http://localhost:9876
KEY=<your-api-key>

# Poll until the workflow reaches a terminal state
while true; do
  RESP=$(curl -s "$SERVER/v1/workflows/$CMD" \
    -H "Authorization: Bearer $KEY")
  STATUS=$(echo "$RESP" | jq -r '.status')
  echo "Workflow status: $STATUS"
  [ "$STATUS" = "complete" ] || [ "$STATUS" = "error" ] && break
  sleep 5
done
```

If the endpoint returns HTTP 404 on the first poll, the command either has not started yet or is not a workflow command — treat 404 as "no workflow" and skip the strip. If 404 is returned after a non-404 response, the workflow was removed; stop polling.

The [Remote-bound TUI tabs](10-remote-mode.md#remote-bound-tui-tabs) feature uses this endpoint internally to render the workflow state strip for commands running on a remote API server.

---

## Full example: session lifecycle

```sh
SERVER=http://localhost:9876
KEY=<your-api-key>   # obtained from the startup banner or --refresh-key

# 1. Create a session
SESSION=$(curl -s -X POST "$SERVER/v1/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"workdir":"/home/user/my-project"}' | jq -r .session_id)
echo "Session: $SESSION"

# 2. Submit a command
CMD=$(curl -s -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H "x-awman-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["workflow","./aspec/workflows/implement-feature.md"]}' | jq -r .command_id)
echo "Command: $CMD"

# 3. Poll until done
# Note: The command will first be in 'queued' status while waiting for a worker.
# When a worker claims it, status transitions to 'running', then finally 'done' or 'error'.
while true; do
  RESP=$(curl -s "$SERVER/v1/commands/$CMD" \
    -H "Authorization: Bearer $KEY")
  STATUS=$(echo "$RESP" | jq -r .status)
  POSITION=$(echo "$RESP" | jq -r '.queue_position // "N/A"')
  echo "Status: $STATUS (queue position: $POSITION)"
  [ "$STATUS" = "done" ] || [ "$STATUS" = "error" ] && break
  sleep 10
done

# 4. Retrieve output
curl -s "$SERVER/v1/commands/$CMD/logs" \
  -H "Authorization: Bearer $KEY" | jq -r .output

# 5. Close the session
curl -s -X DELETE "$SERVER/v1/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY"
```

### Example: one-shot exec prompt

For tasks that don't need a persistent session, `exec prompt` can be run directly from the CLI without starting the HTTP server:

```sh
# Run a single one-shot task; output goes to stdout; exit code reflects agent result
awman exec prompt "Fix the failing tests in src/api" --non-interactive

# Combine with shell tools
awman exec prompt "List all TODO comments in the codebase" --non-interactive | tee todos.txt
```

To drive the same task via the API server (so the result is logged and auditable):

```sh
SERVER=http://localhost:9876
KEY=<your-api-key>

SESSION=$(curl -s -X POST "$SERVER/v1/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"workdir":"/home/user/my-project"}' | jq -r .session_id)

CMD=$(curl -s -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H "x-awman-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["prompt","Fix the failing tests","--non-interactive"]}' | jq -r .command_id)

# Poll as usual...
```

---

## Storage layout

Everything API mode writes lives under `~/.awman/api/`:

```
~/.awman/api/
  awman.log                        # server log (background mode only)
  awman.pid                        # PID file for the background process
  awman.db                         # SQLite database: sessions + commands
  api_key.hash                     # SHA-256 hex digest of the API key (mode 0600)
  sessions/
    <session-uuid>/
      commands/
        <command-uuid>/
          output.log               # combined stdout+stderr (written incrementally)
          metadata.json            # request payload, timestamps, exit code
          workflow.state.json      # workflow state — only present for workflow commands
```

`workflow.state.json` is written and updated atomically (write to a temp file, then rename) each time the workflow advances to a new step. The file uses the identical JSON structure as the local workflow state in `$GITROOT/.awman/workflows/`. It is created only when the command runs a workflow; it is never present for `exec prompt`, `chat`, `implement` (without `--workflow`), or other non-workflow commands.

`awman.db` contains two tables:

**`sessions`** — one row per session: `id` (UUID), `workdir`, `status` (`active`/`closed`), `created_at`, `closed_at`.

**`commands`** — one row per command: `id` (UUID), `session_id`, `subcommand`, `args` (JSON array), `status` (`queued`/`running`/`done`/`error`/`cancelled`), `exit_code`, `started_at`, `finished_at`, `log_path`, `queued_at` (when enqueued), `worker_id` (UUID of the worker that claimed it), and `result` (JSON object with exit code and error message).

The database is the authoritative record of all activity. The per-command log files hold raw output. Neither is deleted when a session is closed.

`metadata.json` for each command contains the original request payload and precise timestamps:

```json
{
  "id": "e5f6a7b8-...",
  "session_id": "a1b2c3d4-...",
  "subcommand": "implement",
  "args": ["0057"],
  "started_at": "2026-04-20T12:01:00Z",
  "finished_at": "2026-04-20T12:43:17Z",
  "exit_code": 0
}
```

---

## Configuration

API mode settings live under an `api` key in the global config (`~/.awman/config.json`). All fields are optional.

```json
{
  "api": {
    "workDirs": [
      "/home/user/my-project",
      "/home/user/other-project"
    ],
    "alwaysNonInteractive": true
  }
}
```

### `api.workDirs`

Pre-configure working directories so you don't have to repeat `--workdirs` every time you start the server:

```sh
awman config set --global api.workDirs "/home/user/my-project,/home/user/other-project"
```

Paths from `api.workDirs` and paths from `--workdirs` flags are merged at startup — both sources can be used together. See [Configuration](07-configuration.md#global-config) for the full global config reference.

### `api.alwaysNonInteractive`

When set to `true`, awman automatically injects `--non-interactive` into every dispatched command that supports it — including `implement`, `chat`, `exec prompt`, `exec workflow`, `ready`, and `specs amend`.

```sh
awman config set --global api.alwaysNonInteractive true
```

This is the recommended setting for API server deployments where no TTY is available. It guarantees that no command blocks waiting for interactive input.

When `alwaysNonInteractive` is `true` and a command is dispatched via the HTTP API, the flag is automatically injected into the args vector — you do not need to include `--non-interactive` in your API requests explicitly.

The setting defaults to `false` so that awman's interactive defaults remain unchanged for users who have not configured an API server.

---

## Security

API mode preserves all of awman's container isolation guarantees: every subcommand runs inside a Docker container, never directly on the host.

The HTTP server enforces cryptographic API key authentication on every request by default. The plaintext key is shown once at server start and never logged or persisted; only its SHA-256 hash is stored on disk. See [Authentication](#authentication) for the full key lifecycle.

The working directory allowlist is the secondary access control on the server: even a client that presents a valid API key can only create sessions in pre-approved directories.

For additional defense in depth, bind the server to `localhost` (the default) and restrict external access via firewall rules or SSH tunnels. Even with authentication enabled, reducing the attack surface is always worthwhile.

---

## Session cleanup

At server startup, awman automatically purges sessions that were closed more than 24 hours ago. Their rows are removed from `awman.db` along with all associated command records. The on-disk output logs in `~/.awman/api/sessions/<uuid>/` are **not** deleted — they remain for audit purposes.

The cleanup runs once, before the server begins accepting connections. Each purged session is logged at `INFO` level:

```
INFO running stale closed session cleanup
INFO deleted stale session a1b2c3d4-... and 3 linked command records
INFO deleted stale session b2c3d4e5-... and 1 linked command records
```

Sessions closed within the last 24 hours are not touched. The 24-hour boundary (`< datetime('now', '-24 hours')`) is exclusive, so a session closed exactly 24 hours ago is not deleted until the next second boundary passes.

---

## Graceful shutdown

On `SIGTERM` or `SIGINT`, the server finishes all in-flight HTTP responses and allows running commands up to 30 seconds to complete before force-terminating them. Both shutdown start and completion are logged. The 30-second grace period applies whether the server was stopped by `awman api kill` or by sending the signal directly.

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| `workdir` not in allowlist on `POST /v1/sessions` | HTTP 403; response includes the list of allowed directories |
| Session not found or already closed on `POST /v1/commands` | HTTP 404; response includes the session UUID |
| Multiple `POST /v1/commands` in quick succession | All commands are enqueued; first returns immediately, subsequent requests also return immediately with different command IDs; all are processed in FIFO order (one per session at a time) |
| Session is `closing` on `POST /v1/commands` | HTTP 409 `"session is closing"`; command is rejected to prevent new work during graceful shutdown |
| Server already running when `api start` is invoked | Error printed; exits non-zero |
| Port already bound (EADDRINUSE) | Error includes the port number and PID holding it |
| `--workdirs` path doesn't exist at startup | Warning logged; path remains on allowlist |
| `awman api kill` when server is not running | Informational message; exits 0 |
| `awman api logs` with no log file | Clear error suggesting `--background` |
| Unknown `subcommand` in `POST /v1/commands` | HTTP 400; response lists valid subcommands |
| `x-awman-session` header missing | HTTP 400 |
| `exec prompt` with empty string | CLI validation error: `"prompt cannot be empty"` before any container launches |
| `exec workflow` with `{{work_item_content}}` and no `--work-item` | Warning printed; placeholder left unexpanded; workflow continues |
| `exec workflow --work-item <N>` where file not found | Error pointing to the expected path pattern; same message as `implement` |
| `api.alwaysNonInteractive` true + duplicate `--non-interactive` flag in args | Flag is deduplicated; no error |
| `exec` dispatched via HTTP API with unknown action (not `prompt`/`workflow`/`wf`) | HTTP 400; response lists valid exec actions |
| `remote` subcommand dispatched via HTTP API without required args (e.g. no `--session`) | Subprocess exits with a clear error; output appears in the command log |
| No `Authorization` header on any request | HTTP 401 with JSON body explaining which header to use |
| Wrong API key on any request | HTTP 401 with JSON `{"error": "Invalid API key."}` |
| `--refresh-key` used: existing clients present the old key | HTTP 401; clients must be updated with the new key |
| `--dangerously-skip-auth` with a hash file on disk | Auth disabled for this run only; the hash file is not modified; next normal start re-enables auth |
| `api_key.hash` file is missing or unreadable at startup | Server treats it as a first start and generates a new key |
| `GET /v1/sessions?status=active` with no active sessions | Returns `{"sessions":[]}` (empty list); HTTP 200 |
| Session transitions to closed between list fetch and command dispatch | Server enforces rejection at `POST /v1/commands` regardless of filter; client receives HTTP 404 |
| Startup cleanup: sessions closed exactly 24h ago | Not deleted until the next second boundary passes (exclusive `<` comparison) |
| Startup cleanup: on-disk log files for cleaned sessions | Not deleted; they remain in `~/.awman/api/sessions/<uuid>/` for audit purposes |
| `GET /v1/workflows/:command_id` — unknown command ID | HTTP 404 `{"error": "command not found"}` |
| `GET /v1/workflows/:command_id` — command is not a workflow command | HTTP 404 `{"error": "no workflow for this command"}`; `workflow.state.json` is never created |
| `GET /v1/workflows/:command_id` — command is `pending` (not yet started) | HTTP 404 `{"error": "no workflow for this command"}`; file does not exist yet |
| `GET /v1/workflows/:command_id` — workflow is paused | HTTP 200; `WorkflowState.status` is `"paused"`; the paused step is identified in the body; polling may continue since the workflow can resume |
| `GET /v1/workflows/:command_id` — workflow is complete | HTTP 200; `WorkflowState.status` is `"complete"`; all steps present with terminal statuses; clients should stop polling |
| Concurrent reads of `workflow.state.json` during a write | File is written atomically via rename; clients never observe a partial JSON document |
| `DELETE /v1/sessions/:id` — no queued commands, no running command | HTTP 200; session immediately transitions to `closed` status |
| `DELETE /v1/sessions/:id` — queued commands present, no running command | HTTP 200; all queued commands are cancelled; session immediately transitions to `closed` status |
| `DELETE /v1/sessions/:id` — running command present | HTTP 202; session transitions to `closing` status; running command is allowed to finish; client must poll `GET /v1/sessions/:id/status` to observe the transition to `closed` |
| `DELETE /v1/sessions/:id` — called again while `closing` | HTTP 200; returns the current session state (still `closing` if command hasn't finished, or `closed` if it has) |
| `GET /v1/sessions/:id/queue` — session has no commands | HTTP 200; returns `queue_depth: 0`, `running: null`, empty arrays for `queued` and `recent_completed` |
| `GET /v1/sessions/:id/queue` — command is `pending` (queued but not yet claimed) | Appears in the `queued` array with a `position` indicating its place in the queue (0-indexed) |
| `POST /v1/commands` with workflow file that doesn't exist | Command is enqueued; when the worker executes it, it fails with an error in the `result` field and status becomes `error` |

---

[← Overlays](08-overlays.md) · [Remote Mode →](10-remote-mode.md)
