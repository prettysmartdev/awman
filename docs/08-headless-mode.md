# Headless Mode

Headless mode exposes amux's session and subcommand execution over HTTP. Start a persistent server with `amux headless start`, then drive sessions and subcommands from scripts, CI pipelines, or any HTTP client — no interactive terminal or TUI required.

A **session** in headless mode is conceptually identical to a TUI tab: a named, isolated workspace bound to a working directory. Subcommands dispatched to a session (`exec workflow`, `chat`, etc.) execute exactly as they would in a TUI tab — inside a Docker container, with all the same security and isolation guarantees.

All operations, inputs, and outputs are recorded durably in `~/.amux/headless/` for auditability.

---

## When to use headless mode

Headless mode is useful for:

- CI pipelines that trigger `exec workflow` or `exec prompt` runs and poll for results
- Scripts or tooling that execute workflows and retrieve output programmatically
- Remote integrations where the amux server runs on one machine and clients run elsewhere
- Audit-heavy environments where a complete durable record of every agent action is required
- One-shot agent invocations from scripts using `amux exec prompt` or `amux exec workflow`

For single interactive sessions, use `amux chat` instead.

---

## One-shot scripted execution (`exec`)

The `exec` subcommand group provides two commands for running agent tasks non-interactively from scripts, CI pipelines, or the headless HTTP server — without a persistent session or TUI.

### `amux exec prompt <prompt>`

Launches an agent container with a pre-supplied prompt. Behaves identically to `amux chat`, except the initial prompt is baked into the launch arguments rather than requiring a live terminal session.

```sh
# Run a single task and exit
amux exec prompt "Fix the failing tests in src/api"

# Non-interactive: agent executes and exits; output goes to stdout
amux exec prompt "Summarise recent changes" --non-interactive

# Use a specific agent and model
amux exec prompt "Refactor the auth module" --agent codex --model gpt-4o

# Full autonomous run
amux exec prompt "Implement caching for the API layer" --yolo --non-interactive
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

### `amux exec workflow <path>` / `amux exec wf <path>`

Runs a workflow file. The work item is optional — when provided, it's used for template variable substitution within the workflow.

```sh
# Run a workflow without a work item
amux exec workflow ./aspec/workflows/implement-feature.md

# Alias: exec wf
amux exec wf ./aspec/workflows/implement-feature.md

# Optionally associate a work item for template variable substitution
amux exec workflow ./aspec/workflows/implement-feature.md --work-item 0053

# Non-interactive workflow run
amux exec workflow ./aspec/workflows/review.md --non-interactive
```

`exec workflow` and `exec wf` are identical — `wf` is a short alias.

**Work item template variables:** When no `--work-item` is given, prompt templates that use `{{work_item_number}}`, `{{work_item_content}}`, or `{{work_item_section:[Name]}}` are left unexpanded with a warning:

```
warning: workflow uses {{work_item_content}} but no --work-item was provided; placeholder left unexpanded
```

When `--work-item <N>` is provided, amux resolves the work item file from the configured work items directory and substitutes all template variables.

**Workflow state files:** When no work item is given, the state file is keyed by the workflow file's name and content hash:

```
~/.amux/headless/<workflow-name>-<content-hash8>.state.json
```

When a work item is given, the state file is saved to:

```
$GITROOT/.amux/workflows/<repo-hash8>-<work-item>-<workflow-name>.json
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
amux headless start --port 9876 --workdirs /path/to/repo
```

The server starts on the specified port (default `9876`) and accepts HTTP requests for the life of the process. Logs are emitted to stdout in human-readable format. Press `Ctrl+C` to stop.

```sh
# Multiple working directories
amux headless start --workdirs /repo-a --workdirs /repo-b

# Custom port
amux headless start --port 8080 --workdirs /repo

# Rotate the API key and print the new one to stdout
amux headless start --refresh-key --port 9876 --workdirs /repo

# Disable authentication for this run (WARNING: anyone can reach the server)
amux headless start --dangerously-skip-auth --port 9876 --workdirs /repo
```

`--workdirs` accepts one or more absolute paths (repeat the flag for multiple values). Only working directories on the allowlist can be used to create sessions — requests with any other path are rejected with HTTP 403. See [Working directory allowlist](#working-directory-allowlist).

**Start flags related to authentication:**

| Flag | Description |
|------|-------------|
| `--refresh-key` | Generate a new API key, write its hash to disk, and print the plaintext key to stdout. The old key is invalidated immediately. |
| `--dangerously-skip-auth` | Disable authentication for this process lifetime only. The `api_key.hash` file is left untouched; the next normal start re-enables auth using the stored hash. |

### Background

```sh
amux headless start --background --port 9876 --workdirs /path/to/repo
```

`--background` daemonizes the server using the OS process manager:

| Platform | Mechanism |
|----------|-----------|
| Linux (systemd available) | `systemd-run --user` writes a transient unit |
| macOS (launchd) | Writes `~/Library/LaunchAgents/io.amux.headless.plist` and calls `launchctl load` |
| Fallback (no systemd/launchd) | Double-fork; PID written directly |

The PID is stored at `~/.amux/headless/amux.pid`. Logs go to `~/.amux/headless/amux.log`.

If a server is already running (detected via `amux.pid` and a live process check), `start` prints an error and exits with a non-zero code rather than silently competing for the port.

If `bind()` fails because the port is already in use, the error message includes the port number and the PID holding it (when discoverable):

```
error: port 9876 is already in use (PID 41290)
```

---

## Server lifecycle commands

### Status

```sh
amux headless status
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
amux headless logs
```

Streams `~/.amux/headless/amux.log` to stdout in real time (equivalent to `tail -f`). Only available when the server was started with `--background`. Press `Ctrl+C` to stop streaming.

If no log file exists:

```
error: no log file found at ~/.amux/headless/amux.log
       start the server with --background to enable file logging
```

### Kill

```sh
amux headless kill
```

Sends `SIGTERM` to the background server process, allows in-flight requests to drain (up to the graceful shutdown period), and removes the PID file. On macOS, also unloads the launchd plist.

If the server is not running:

```
info: server is not running (no PID file found)
```

---

## Authentication

The headless server uses cryptographic API key authentication. Every HTTP request must include the API key; unauthenticated requests are rejected with HTTP 401 before reaching any handler.

### First start — key generation

On the first start (when no `api_key.hash` file exists in `~/.amux/headless/`), amux automatically generates a cryptographically random 32-byte key, stores only its SHA-256 hash on disk (never the plaintext), and prints the key to stdout once:

```
╔═══════════════════════════════════════════════════════════════════╗
║  amux headless API key (store this — it will not be shown again)  ║
║  a3f8b2c1...64-character-hex-key...d7e9f0a1                       ║
╚═══════════════════════════════════════════════════════════════════╝
```

**This is the only time the plaintext key appears.** Store it immediately. Key generation happens before the log file is opened, so the key cannot appear in `amux.log` under any circumstances.

### Subsequent starts

On subsequent starts (when `~/.amux/headless/api_key.hash` already exists), the server loads the stored hash silently and starts normally — no banner is printed. The same key continues to work without any client-side changes.

### Rotating the key — `--refresh-key`

To invalidate the current key and generate a new one:

```sh
amux headless start --refresh-key --port 9876 --workdirs /repo
```

A new key is generated and its hash replaces the file on disk. The new plaintext key is printed to stdout using the same banner format. All clients using the old key will immediately receive HTTP 401 and must be updated.

### Disabling authentication — `--dangerously-skip-auth`

```sh
amux headless start --dangerously-skip-auth --port 9876 --workdirs /repo
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

When using `amux remote` subcommands, the key is resolved and injected automatically — see [Remote Mode: API key](09-remote-mode.md#api-key-authentication).

**Error responses from the middleware:**

| Situation | HTTP status | Response body |
|-----------|-------------|---------------|
| No `Authorization` header | 401 | `{"error": "API key required. Pass the key via the Authorization header (e.g. Authorization: Bearer <key>)."}` |
| Wrong key | 401 | `{"error": "Invalid API key."}` |

Hash comparisons use constant-time comparison (`ring::constant_time::verify_slices_are_equal`) to prevent timing attacks.

### Key storage

The key hash is stored at `~/.amux/headless/api_key.hash` with mode `0o600` (owner read/write only on Unix). Only the SHA-256 hex digest is written — the plaintext key is never persisted anywhere.

---

## Working directory allowlist

The server maintains a strict allowlist of working directories. Any session creation request that specifies a path not on the allowlist is rejected with HTTP 403.

**At startup**, the allowlist is populated from two sources:

1. `--workdirs` flags passed to `amux headless start`
2. `headless.workDirs` in the global config (`~/.amux/config.json`)

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

## HTTP API

All endpoints speak JSON. All requests and responses are logged at `INFO` level or above.

When authentication is enabled (the default), every request must include the API key in the `Authorization` header. See [Authentication](#authentication) for details.

### Base URL

```
http://localhost:<port>/v1
```

### Endpoint reference

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/workdirs` | List the server's allowlisted working directories |
| `POST` | `/v1/sessions` | Create a new session |
| `GET` | `/v1/sessions` | List sessions; accepts optional `?status=active` filter |
| `GET` | `/v1/sessions/:id` | Get session detail |
| `DELETE` | `/v1/sessions/:id` | Close a session |
| `POST` | `/v1/commands` | Submit a subcommand to a session |
| `GET` | `/v1/commands/:id` | Get command status and metadata |
| `GET` | `/v1/commands/:id/logs` | Get captured command output (snapshot) |
| `GET` | `/v1/commands/:id/logs/stream` | Stream live command output via Server-Sent Events |
| `GET` | `/v1/workflows/:id` | Get the workflow state for a command |
| `GET` | `/v1/status` | Server health (uptime, active sessions, running commands) |

---

### Sessions

#### Create a session

```sh
curl -s -X POST http://localhost:9876/v1/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"workdir":"/home/user/my-project"}'
```

Creates a new session bound to the given working directory. The directory must be in the allowlist. Returns immediately with a session UUID:

```json
{ "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890" }
```

Error responses:

| Situation | HTTP status |
|-----------|-------------|
| `workdir` not in allowlist | 403 — includes `allowed_workdirs` list |
| `workdir` field missing | 400 |

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

Marks the session `closed`. Closed sessions cannot receive new commands. All existing command records and output files are preserved — no data is deleted. Returns HTTP 204 on success, HTTP 404 if the session does not exist.

---

### Commands

Commands are submitted to a session and execute asynchronously. Submit a command and receive a `command_id` immediately; poll the command endpoint to track progress and retrieve output.

#### Submit a command

```sh
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'x-amux-session: <session-id>' \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"chat"}'
```

Dispatches a subcommand to the session identified by the `x-amux-session` header. Valid values for `subcommand`: `chat`, `ready`, `exec`, `remote`.

For `exec`, the `args` array starts with the exec action (`prompt` or `workflow`/`wf`), followed by any further arguments:

```sh
# exec prompt via headless API
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'x-amux-session: <session-id>' \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["prompt","Fix the failing tests","--non-interactive"]}'

# exec workflow via headless API
curl -s -X POST http://localhost:9876/v1/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'x-amux-session: <session-id>' \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["workflow","./aspec/workflows/implement-feature.md","--work-item","0053"]}'
```

Returns immediately with a command UUID — execution is asynchronous:

```json
{ "command_id": "e5f6a7b8-..." }
```

**One command at a time per session.** If a command is already running in the session, the request returns HTTP 403:

```json
{
  "error": "session busy",
  "running_command_id": "e5f6a7b8-..."
}
```

Once the running command completes, the session accepts new commands.

Error responses:

| Situation | HTTP status | `error` field |
|-----------|-------------|---------------|
| Session not found or closed | 404 | `"session not found"` (includes session UUID) |
| Another command is running | 403 | `"session busy"` |
| Unknown subcommand | 400 | `"unknown subcommand"` (includes list of valid subcommands) |
| `x-amux-session` header missing | 400 | `"missing x-amux-session header"` |

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
  "subcommand": "implement",
  "args": ["0057"],
  "status": "running",
  "exit_code": null,
  "started_at": "2026-04-20T12:01:00Z",
  "finished_at": null,
  "log_path": "~/.amux/headless/sessions/a1b2c3d4-.../commands/e5f6a7b8-.../output.log"
}
```

| `status` value | Meaning |
|----------------|---------|
| `pending` | Accepted; not yet started |
| `running` | Container is executing |
| `done` | Completed with exit code 0 |
| `error` | Completed with a non-zero exit code |

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

Opens a persistent HTTP response using [Server-Sent Events (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events). The server replays any output already written, then tails the log file and sends new lines as they arrive. When the command completes, the server sends a `[amux:done]` sentinel event and closes the response.

**SSE event format:**

```
data: <line of log output>

data: <another line>

data: [amux:done]

```

Each event is terminated by a blank line (standard SSE format). The sentinel `[amux:done]` signals that the command has finished — no more output will follow.

**Shell example — stream and print until done:**

```sh
curl -s http://localhost:9876/v1/commands/<command-id>/logs/stream \
  -H 'Authorization: Bearer <api-key>' \
| while IFS= read -r line; do
    case "$line" in
      "data: [amux:done]") echo "--- done ---"; break ;;
      data:\ *)             echo "${line#data: }" ;;
    esac
  done
```

**Behaviour notes:**

- If the command has already completed when you connect, the server replays the full historical log and sends `[amux:done]` immediately — no output is missed.
- If the client disconnects mid-stream, the command continues executing unaffected.
- If the log file does not yet exist (the command is `pending`), the server waits up to 10 s for it to appear before returning HTTP 404.
- The `Content-Type` response header is `text/event-stream`.
- **Read timeout:** `amux remote run --follow` uses a 10-minute read timeout per SSE event. Any output from the server resets the timer, so long-running commands that produce incremental output can stream for hours. If the server is completely silent for 10 minutes, the client disconnects with a timeout message; the command continues running on the server. When using cURL directly with `--no-buffer`, consider adding `--max-time 0` to disable cURL's own timeout for very long-running commands.

`amux remote run --follow` uses this endpoint internally. The cURL form above is equivalent and is useful in scripts where the amux binary is unavailable on the client.

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

When a command runs a workflow (`exec workflow`), the headless server writes a `workflow.state.json` file to the per-command directory. This file is updated atomically on every step transition. The `GET /v1/workflows/:command_id` endpoint exposes that state over HTTP.

#### Get workflow state

```sh
curl -s http://localhost:9876/v1/workflows/<command-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current `WorkflowState` for the given command. The structure is identical to the local workflow state format produced by `amux exec workflow` when it writes state to `$GITROOT/.amux/workflows/`.

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

The [Remote-bound TUI tabs](09-remote-mode.md#remote-bound-tui-tabs) feature uses this endpoint internally to render the workflow state strip for commands running on a remote headless server.

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
  -H "x-amux-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["workflow","./aspec/workflows/implement-feature.md"]}' | jq -r .command_id)
echo "Command: $CMD"

# 3. Poll until done
while true; do
  STATUS=$(curl -s "$SERVER/v1/commands/$CMD" \
    -H "Authorization: Bearer $KEY" | jq -r .status)
  echo "Status: $STATUS"
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
amux exec prompt "Fix the failing tests in src/api" --non-interactive

# Combine with shell tools
amux exec prompt "List all TODO comments in the codebase" --non-interactive | tee todos.txt
```

To drive the same task via the headless HTTP server (so the result is logged and auditable):

```sh
SERVER=http://localhost:9876
KEY=<your-api-key>

SESSION=$(curl -s -X POST "$SERVER/v1/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"workdir":"/home/user/my-project"}' | jq -r .session_id)

CMD=$(curl -s -X POST "$SERVER/v1/commands" \
  -H "Authorization: Bearer $KEY" \
  -H "x-amux-session: $SESSION" \
  -H 'Content-Type: application/json' \
  -d '{"subcommand":"exec","args":["prompt","Fix the failing tests","--non-interactive"]}' | jq -r .command_id)

# Poll as usual...
```

---

## Storage layout

Everything headless mode writes lives under `~/.amux/headless/`:

```
~/.amux/headless/
  amux.log                         # server log (background mode only)
  amux.pid                         # PID file for the background process
  amux.db                          # SQLite database: sessions + commands
  api_key.hash                     # SHA-256 hex digest of the API key (mode 0600)
  sessions/
    <session-uuid>/
      commands/
        <command-uuid>/
          output.log               # combined stdout+stderr (written incrementally)
          metadata.json            # request payload, timestamps, exit code
          workflow.state.json      # workflow state — only present for workflow commands
```

`workflow.state.json` is written and updated atomically (write to a temp file, then rename) each time the workflow advances to a new step. The file uses the identical JSON structure as the local workflow state in `$GITROOT/.amux/workflows/`. It is created only when the command runs a workflow; it is never present for `exec prompt`, `chat`, `implement` (without `--workflow`), or other non-workflow commands.

`amux.db` contains two tables:

**`sessions`** — one row per session: `id` (UUID), `workdir`, `status` (`active`/`closed`), `created_at`, `closed_at`.

**`commands`** — one row per command: `id` (UUID), `session_id`, `subcommand`, `args` (JSON array), `status` (`pending`/`running`/`done`/`error`), `exit_code`, `started_at`, `finished_at`, `log_path`.

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

Headless mode settings live under a `headless` key in the global config (`~/.amux/config.json`). All fields are optional.

```json
{
  "headless": {
    "workDirs": [
      "/home/user/my-project",
      "/home/user/other-project"
    ],
    "alwaysNonInteractive": true
  }
}
```

### `headless.workDirs`

Pre-configure working directories so you don't have to repeat `--workdirs` every time you start the server:

```sh
amux config set --global headless.workDirs "/home/user/my-project,/home/user/other-project"
```

Paths from `headless.workDirs` and paths from `--workdirs` flags are merged at startup — both sources can be used together. See [Configuration](07-configuration.md#global-config) for the full global config reference.

### `headless.alwaysNonInteractive`

When set to `true`, amux automatically injects `--non-interactive` into every dispatched command that supports it — including `implement`, `chat`, `exec prompt`, `exec workflow`, `ready`, and `specs amend`.

```sh
amux config set --global headless.alwaysNonInteractive true
```

This is the recommended setting for headless server deployments where no TTY is available. It guarantees that no command blocks waiting for interactive input.

When `alwaysNonInteractive` is `true` and a command is dispatched via the HTTP API, the flag is automatically injected into the args vector — you do not need to include `--non-interactive` in your API requests explicitly.

The setting defaults to `false` so that amux's interactive defaults remain unchanged for users who have not configured a headless server.

---

## Security

Headless mode preserves all of amux's container isolation guarantees: every subcommand runs inside a Docker container, never directly on the host.

The HTTP server enforces cryptographic API key authentication on every request by default. The plaintext key is shown once at server start and never logged or persisted; only its SHA-256 hash is stored on disk. See [Authentication](#authentication) for the full key lifecycle.

The working directory allowlist is the secondary access control on the server: even a client that presents a valid API key can only create sessions in pre-approved directories.

For additional defense in depth, bind the server to `localhost` (the default) and restrict external access via firewall rules or SSH tunnels. Even with authentication enabled, reducing the attack surface is always worthwhile.

---

## Session cleanup

At server startup, amux automatically purges sessions that were closed more than 24 hours ago. Their rows are removed from `amux.db` along with all associated command records. The on-disk output logs in `~/.amux/headless/sessions/<uuid>/` are **not** deleted — they remain for audit purposes.

The cleanup runs once, before the server begins accepting connections. Each purged session is logged at `INFO` level:

```
INFO running stale closed session cleanup
INFO deleted stale session a1b2c3d4-... and 3 linked command records
INFO deleted stale session b2c3d4e5-... and 1 linked command records
```

Sessions closed within the last 24 hours are not touched. The 24-hour boundary (`< datetime('now', '-24 hours')`) is exclusive, so a session closed exactly 24 hours ago is not deleted until the next second boundary passes.

---

## Graceful shutdown

On `SIGTERM` or `SIGINT`, the server finishes all in-flight HTTP responses and allows running commands up to 30 seconds to complete before force-terminating them. Both shutdown start and completion are logged. The 30-second grace period applies whether the server was stopped by `amux headless kill` or by sending the signal directly.

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| `workdir` not in allowlist on `POST /v1/sessions` | HTTP 403; response includes the list of allowed directories |
| Session not found or already closed on `POST /v1/commands` | HTTP 404; response includes the session UUID |
| Second `POST /v1/commands` while one is running | HTTP 403 `"session busy"`; includes running command ID |
| Server already running when `headless start` is invoked | Error printed; exits non-zero |
| Port already bound (EADDRINUSE) | Error includes the port number and PID holding it |
| `--workdirs` path doesn't exist at startup | Warning logged; path remains on allowlist |
| `amux headless kill` when server is not running | Informational message; exits 0 |
| `amux headless logs` with no log file | Clear error suggesting `--background` |
| Unknown `subcommand` in `POST /v1/commands` | HTTP 400; response lists valid subcommands |
| `x-amux-session` header missing | HTTP 400 |
| `exec prompt` with empty string | CLI validation error: `"prompt cannot be empty"` before any container launches |
| `exec workflow` with `{{work_item_content}}` and no `--work-item` | Warning printed; placeholder left unexpanded; workflow continues |
| `exec workflow --work-item <N>` where file not found | Error pointing to the expected path pattern; same message as `implement` |
| `headless.alwaysNonInteractive` true + duplicate `--non-interactive` flag in args | Flag is deduplicated; no error |
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
| Startup cleanup: on-disk log files for cleaned sessions | Not deleted; they remain in `~/.amux/headless/sessions/<uuid>/` for audit purposes |
| `GET /v1/workflows/:command_id` — unknown command ID | HTTP 404 `{"error": "command not found"}` |
| `GET /v1/workflows/:command_id` — command is not a workflow command | HTTP 404 `{"error": "no workflow for this command"}`; `workflow.state.json` is never created |
| `GET /v1/workflows/:command_id` — command is `pending` (not yet started) | HTTP 404 `{"error": "no workflow for this command"}`; file does not exist yet |
| `GET /v1/workflows/:command_id` — workflow is paused | HTTP 200; `WorkflowState.status` is `"paused"`; the paused step is identified in the body; polling may continue since the workflow can resume |
| `GET /v1/workflows/:command_id` — workflow is complete | HTTP 200; `WorkflowState.status` is `"complete"`; all steps present with terminal statuses; clients should stop polling |
| Concurrent reads of `workflow.state.json` during a write | File is written atomically via rename; clients never observe a partial JSON document |

---

[← Configuration](07-configuration.md) · [Remote Mode →](09-remote-mode.md)
