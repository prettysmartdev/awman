# API & Remote Mode

awman can be driven over HTTP instead of from an interactive terminal. This guide covers both halves of that:

- **[API mode](#api-mode)** — the daemon's worker feature accepts pushed sessions and commands under `/v1/worker/*`, so scripts, CI pipelines, and HTTP clients can drive them. This half also covers [non-interactive (headless) operation](#non-interactive-headless-operation), which applies to plain CLI use too.
- **[Remote mode](#remote-mode)** — the *client*. `awman remote …` is a thin command-line client for an awman API server, with live log streaming, so you can dispatch work to a build host as if it were local.

You do not need remote mode to use the API (curl works fine), and you do not need to understand the HTTP surface to use `awman remote`. Read whichever half you need.

---

# API mode

API mode is the push side of the daemon's worker feature. Enable the worker feature, start the daemon, then send sessions and commands under `/v1/worker/*` from scripts, CI pipelines, or HTTP clients — no interactive terminal or TUI required.

`awman link` saves a connection to a separate work server; it does not
turn that server into an API target or start pulling jobs. See
[Workers and Work Servers](17-workers-and-work-servers.md).

The API and `awman remote` operate on the **push** side of the worker feature:
clients submit work to an awman daemon under `/v1/worker/*`. Work-server pull
routes also use `/v1/worker/*`, but they are local-listener routes and are not
exposed through the network API. The daemon registers linked servers and runs
pulled leases; `503 pull_disabled` means pull is disabled or draining. See
[Workers and Work Servers](17-workers-and-work-servers.md) for the work-server
protocol and configuration.

A **session** in API mode is conceptually identical to a TUI tab: an isolated workspace bound to a working directory. Commands dispatched to a session (`exec prompt`, `exec workflow`, and the others listed under [Submit a command](#submit-a-command)) execute exactly as they would in a TUI tab — inside a container on the configured runtime, with the same security and isolation guarantees.

Worker sessions, commands, and output are stored under the awman home directory for auditability. The shared SQLite database is stored at
`$AWMAN_HOME/data/awman.db`.

API mode is for request-driven sessions and commands. To hand recurring work to
a group of agents that pick it up on their own, build a [squad](12-squad.md)
instead. Worker and squad are features of one daemon and can run together.

---

## When to use API mode

API mode is useful for:

- CI pipelines that trigger `exec workflow` or `exec prompt` runs and poll for results
- Scripts or tooling that execute workflows and retrieve output programmatically
- Remote integrations where the awman server runs on a build host and clients reach it through a tunnel or proxy
- Audit-heavy environments where a complete durable record of every agent action is required

For single interactive sessions, use `awman chat` instead.

---

## API conventions

These rules hold for every route in the [route tree](#route-tree).

### Versioning and base path

All routes live under `/v1`. `GET /v1/status` reports `protocol: {"api": 1}` and the `capabilities` your server supports, so a client can test for a capability instead of comparing versions.

### Resource IDs

Every ID is a short prefix, an underscore and 32 lowercase hex characters, for example `cmd_e5f6a7b8e5f6a7b8e5f6a7b8e5f6a7b8`. Treat IDs as opaque strings; the prefix tells you what kind of resource it names.

| Prefix | Resource |
|--------|----------|
| `ses_` | Worker session |
| `cmd_` | Command |
| `task_` | Squad task |
| `run_` | Squad run |
| `grant_` | Remembered prerequisite grant |
| `term_` | Daemon terminal |
| `ins_`, `tab_`, `lease_`, `key_` | Engine instance, tab, work lease, or API key identifiers |

An ID that is malformed or has the wrong prefix is answered exactly like an ID that does not exist: `404 not_found`.

### List responses

Every `GET` that returns more than one record answers with an envelope:

```json
{ "items": [ … ], "next_cursor": "eyJ…" }
```

Pass `?limit=` (default 50, maximum 500; `0` or more than 500 is `400 invalid_request`) and `?cursor=` (the previous `next_cursor`). `next_cursor` is `null` on the last page. Cursors are opaque — do not build or parse them; a malformed cursor is `400 invalid_request`.

### Errors

Every error is a JSON body with a human-readable `error` and a stable `code`:

```json
{ "error": "Session 'ses_…' not found", "code": "session_not_found" }
```

Match on `code`. The full code table is under [Error responses](#error-responses). A route whose [feature](#features-and-access) is switched off answers `503 feature_disabled`.

### Times, durations and sizes

- Timestamps end in `_at` and are RFC 3339 UTC with milliseconds: `"2026-11-05T12:34:56.789Z"`.
- Durations are integer milliseconds in fields ending in `_ms` (`uptime_ms`, `interval_ms`).
- Sizes are integer bytes in fields ending in `_bytes`.

(Duration and size *settings* in config files are strings such as `"30s"` and `"4GiB"`; see [Configuration](07-configuration.md).)

### Labels

Sessions and commands accept [labels](#labels). A label key is `<namespace>/<name>` in lowercase `[a-z0-9.-]`; the `awman/` namespace is reserved. Labels stored before this rule stay readable and filterable.

### `GET /v1/status`

```json
{
  "status": "ok",
  "version": "0.13.0",
  "protocol": { "api": 1 },
  "capabilities": ["daemon_features", "network_listener", "api_keys", "health", "trace_propagation", "…"],
  "features": {
    "worker": {
      "name": "worker", "state": "on", "enabled_by": "config",
      "max_concurrent_jobs": 2, "running": 1, "queued": 0,
      "oldest_queued_age_ms": null, "restarts": 0, "last_error": null,
      "since_at": "2026-11-06T10:00:00Z"
    }
  },
  "max_concurrent_jobs": 4,
  "pid": 1234,
  "uptime_ms": 12345,
  "agents": ["claude", "codex"],
  "listeners": {
    "local": { "transport": "unix_socket", "address": "/home/u/.awman/data/daemon/api.sock" },
    "network": {
      "default": {
        "enabled": true, "listen": "0.0.0.0:9876", "bound": "0.0.0.0:9876", "error": null,
        "transport": "tls", "features": ["worker"], "api_keys": 2,
        "browser_links": false, "browser_grants": 0, "allowed_origins": []
      }
    }
  }
}

```

`capabilities` (a list of strings) is what the older `features` list used to be; `features` is now an object keyed by feature name holding each feature's state. See [Server health](#server-health).

The `listeners.network` object is keyed by listener name. Browser linking is
available through the built-in loopback listener when it serves the engine and
allows the requesting origin. Other listener configurations may report
`browser_links: false`.

### Features and access

Each network listener serves only the features listed in its
`daemon.network.listeners.<name>.features` setting and enabled by config.
Ephemeral features remain local. Removing a feature from a listener takes
effect on the next request, including requests on existing connections.
Hidden paths return 404 for every method before key authentication.

The daemon is organised into **features**: `core` (status, config, features, grants), `worker` (sessions, commands and runs driven by clients) and `squad` (scheduled tasks). Each route belongs to one feature, and its path says which: `/v1/worker/…`, `/v1/squad/…`. A feature has a state — `on`, `off`, `draining` (no new work accepted) or `ephemeral` — shown by `GET /v1/features`. On the local listener, routes of a feature that is `off` answer `503 feature_disabled`. Network concealment takes precedence and returns `404 not_found` for features that are not exposed. A recovering feature reports `degraded`; reads remain available and creates answer `503 feature_degraded`.

Two access classes apply:

- **Feature routes** are served on every listener that exposes their feature, to an authenticated client.
- **Owner/local routes** (`/v1/config`, `/v1/config/reload`, `/v1/grants`, `/v1/grants/{id}`) are served only on the owner-authenticated local socket. On the network listener they answer `404 not_found` even with a valid API key, so `awman daemon api config.get` works against a local daemon but not through `--remote`.

---

## Quickstart — copy-pastable examples

These examples use the `default` listener at `127.0.0.1:9876` with TLS.
Configure `daemon.network.listeners.default` and create a named API key as
described in [The daemon](16-daemon.md).

```sh
# 0. Enable worker requests, allow this directory, and start the daemon.
awman config set --global daemon.features.worker.enabled true
awman config set --global daemon.features.worker.push.workDirs.allow "$HOME/my-project"
awman config set --global daemon.network.listeners.default.enabled true
awman config set --global daemon.network.listeners.default.listen 127.0.0.1:9876
awman daemon start --background

# Create a named key locally and capture it once:
KEY=$(awman daemon api keys.create name=build-host --json | jq -r .key)
SERVER=https://127.0.0.1:9876
```

### Local session

A local session binds to a directory that's already on the server's allowlist (configured with `daemon.features.worker.push.workDirs.allow`).

**Using `curl`:**

```sh
# 1. Create a session bound to a workdir
SESSION=$(curl -sk -X POST "$SERVER/v1/worker/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"session_type\":\"local\",\"workdir\":\"$HOME/my-project\"}" | jq -r .session_id)

# 2. Wait for session setup (it runs `ready`) to finish
until [ "$(curl -sk "$SERVER/v1/worker/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY" | jq -r .setup_status)" = ready ]; do sleep 2; done

# 3. Dispatch a one-shot prompt; the response carries the command_id
CMD=$(curl -sk -X POST "$SERVER/v1/worker/commands" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec prompt","args":["Summarise recent changes"]}' \
  | jq -r .command_id)

# 4. Stream the output until the command finishes
curl -Nsk "$SERVER/v1/worker/commands/$CMD/events" \
  -H "Authorization: Bearer $KEY" \
  | while IFS= read -r line; do
      case "$line" in
        "event: done") break ;;
        data:\ *)      printf '%s\n' "${line#data: }" ;;
      esac
    done

# 5. Close the session
curl -sk -X DELETE "$SERVER/v1/worker/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY"
```

[`POST /v1/worker/runs`](#one-shot-runs) combines steps 1–3 (and optionally 5) into one request.

**Using `awman` itself** (`awman remote` waits for readiness for you):

```sh
# One-time setup so subsequent commands don't need flags
awman config set --global remote.targets.build.url "$SERVER"
awman config set --global remote.targets.build.apiKey "$KEY"
awman config set --global remote.default build

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
SESSION=$(curl -sk -X POST "$SERVER/v1/worker/sessions" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "session_type": "remote",
    "repo_url": "https://github.com/org/my-project",
    "branch": "main"
  }' | jq -r .session_id)

# 2. Wait for the clone and setup to finish ("failed" means setup failed)
until [ "$(curl -sk "$SERVER/v1/worker/sessions/$SESSION" \
  -H "Authorization: Bearer $KEY" | jq -r .setup_status)" = ready ]; do sleep 5; done

# 3. Run a workflow against the clone
CMD=$(curl -sk -X POST "$SERVER/v1/worker/commands" \
  -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec workflow","args":["aspec/workflows/implement-feature.toml","--work-item","0053"]}' \
  | jq -r .command_id)

# 4. Poll status until terminal
while true; do
  STATUS=$(curl -sk "$SERVER/v1/worker/commands/$CMD" \
    -H "Authorization: Bearer $KEY" | jq -r .status)
  echo "status=$STATUS"
  case "$STATUS" in done|error|cancelled) break ;; esac
  sleep 5
done

# 5. Closing deletes the on-server clone
curl -sk -X DELETE "$SERVER/v1/worker/sessions/$SESSION" \
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
awman exec prompt "Summarise recent changes" --headless

# Use a specific agent and model
awman exec prompt "Refactor the auth module" --agent codex --model gpt-4o

# Send a GitHub issue as the prompt
awman exec prompt --issue 84

# Prepend your own context to the issue
awman exec prompt "Security review for: " --issue 84

# Full autonomous run
awman exec prompt "Implement caching for the API layer" --yolo --headless
```

A prompt, `--issue`, or both is required. With both, your text comes first, followed by the issue content ([GitHub Integration](10-github-integration.md)).

`exec prompt` accepts the same agent flags as `chat` (`--headless`/`-n`, `--plan`, `--auto`, `--yolo`, `--agent`, `--model`, `--overlay`, `--agent-flag`, `--allow-docker`); see [Agent Sessions](03-agent-sessions.md#flags-common-to-chat-and-other-agent-launching-commands) and the [Command Reference](14-command-reference.md#awman-exec-prompt).

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
awman exec workflow ./aspec/workflows/review.toml --headless
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
awman chat --headless
```

When a TTY is present and `--headless` is not passed, awman allows interactive features — full-screen agent sessions, PTY passthrough, and terminal resize (SIGWINCH) forwarding. When no TTY is detected, the agent runs in print/batch mode and awman makes no attempt to manage the terminal.

The worker feature runs pushed commands headless by default, regardless of the daemon process's stdin (`daemon.features.worker.push.interaction`): clients drive it over HTTP and receive updates through the event stream, so there is no terminal to manage.

### Parallel agents in interactive CLI mode

`exec workflow` can run more than one step at once when the workflow's steps are independent (see [Parallel workflows](05-workflows.md#parallel-workflows)). In command mode, running multiple agents at once with a real terminal attached would normally mean several containers writing to your terminal at the same time. Instead, when stdout is a TTY and more than one step is running, awman activates a lightweight status chrome:

- The bottom row of the terminal is reserved for a status line: `[N agents running | showing: <step_name> | Ctrl-S: switch]`
- The rest of the terminal shows the output of whichever step is currently "showing" (the **focused** step)
- Press **Ctrl-S** to cycle the focused step; the display switches to that step's output and the status line updates
- A stuck focused step (no output for 30 seconds) gets a `⚠` prefix on its name in the status line

This chrome is deliberately minimal — a single reserved status row, not a full UI — and only appears when there is something to arbitrate between. With a single running step, or when stdout is not a TTY (redirected to a file, piped, or `--headless`), awman falls back to plain sequential passthrough exactly as described above. In that non-interactive case, if a workflow launches more than one step in parallel, awman prints a warning that step output will interleave without any chrome to separate it, since there is no terminal to reserve a status row on.

### The `--headless` / `-n` flag

Force non-interactive mode even when a TTY is available. Useful for:

- Testing how a workflow behaves headlessly without leaving your terminal.
- Scripting awman commands inside a shell script you happen to be running interactively.
- Environments with a quirky terminal where TTY detection is unreliable.

### Yolo mode in headless contexts

`--yolo` works without a terminal, and its stuck detection keeps unattended runs from stalling: when a step produces no output for the stuck timeout (`agents.stuckTimeout`, default `30s`), a 60-second countdown starts, reported on stderr every 10 seconds, and the step advances when it expires.

```sh
# CI: no explicit --headless needed; awman detects the headless context
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

Headless, CI, and API-server runs work the same way described by `<data>/daemon/server.json`runtime.kind = "builtin"`` (macOS on Apple Silicon only), with these differences:

- The host must be an Apple Silicon Mac running the signed `awman-macos-arm64` binary (a macOS CI runner or VM must itself expose Hypervisor.framework). No Docker daemon is needed to run agents, but images must be available: run `awman ready` (with a configured `runtime.builtin.imageSource`) as a setup step. Unless `runtime.builtin.builder.enabled` is set with a local source, a launch whose image is missing fails with build-and-import instructions; workflow preflight and squad never build.
- Each session's microVM lives and dies with the awman process that launched it. Under `awman daemon start`, that is the daemon; stopping the server stops the VMs it launched.
- `awman ready --json` includes an `image_source` entry when images were imported.

See [Runtimes: Builtin microVM](11-runtimes.md#builtin-microvm-experimental).

---

## Running the API server

API requests are handled by the daemon's worker feature. Enable it, configure
its working-directory allowlist, and start the daemon:

```sh
awman config set daemon.features.worker.enabled true
awman config set daemon.features.worker.push.workDirs.allow "$HOME/my-project"
awman daemon start --background
awman daemon status
```

The daemon always provides an owner-only local listener for local clients.
Remote HTTP access uses a named TLS network listener configured under
`daemon.network.listeners.<name>`. See [The daemon](16-daemon.md) for listener
setup and security. Local clients use the owner-only listener when no remote
target is selected.

Use `awman daemon stop` for a graceful drain, or `awman daemon stop --now` to
cancel running worker and squad jobs before stopping. Read output with
`awman daemon logs` or follow it with `awman daemon logs --follow`.

## Authentication

The network listener requires a named API key on every request. The local
owner-only listener uses operating-system ownership and does not use a bearer
key. Keys are created, listed, and revoked through `awman daemon api`; only
creation returns plaintext key material, and that key is shown once. See [The
daemon](16-daemon.md#local-listener-and-api-management) for key creation and
rotation.

### Network API keys

Create a named key through the daemon's local API client. The returned key is
shown once; `keys.list` never returns key material:

```sh
awman daemon api keys.create name=build-host
awman daemon api keys.list
```

To rotate a key, create a replacement, update the clients, then delete the
previous key by its `key_…` ID:

```sh
awman daemon api keys.delete id=key_…
```

HTTP clients authenticate with `Authorization: Bearer <key>`. Ordinary network
API requests require a named API key. Browser access uses owner-approved grants
on an eligible loopback engine listener. The local owner-only listener does
not need a key.

---

## Working directory allowlist

The server maintains a strict allowlist of working directories. Any session creation request that specifies a path not on the allowlist is rejected with HTTP 403.

There are two kinds of entry:

- **Workdirs** name one directory each. Source: `daemon.features.worker.push.workDirs.allow` in the global config (`$AWMAN_HOME/config.json`).
- **Roots** name a directory that *contains* repositories. Source: `daemon.features.worker.push.workDirs.roots` in the global config. A request is accepted when its `workdir` is a repository cloned **directly** beneath a root — so a client can clone `/srv/repos/app` and create a session on it without restarting the server.

A request is allowed if it matches any workdir entry or any root.

```sh
awman config set --global daemon.features.worker.push.workDirs.roots "/srv/repos,/home/user/projects"
```

A directory under a root is accepted only when all of these hold:

1. It is a direct child of the root — not a grandchild, and not the root itself.
2. It is its own Git root (its top-level directory is the child itself, not a parent repository).
3. Its `.git` is a real directory. A linked worktree (where `.git` is a file) or a symlinked `.git` is refused.

A refused request returns HTTP 403 with a message naming the rule that failed and the path as submitted. Use `GET /v1/worker/workdirs` for the effective allowlist.

A root must not be a Git repository or lie inside one: starting the server with one exits with code 2 and suggests `daemon.features.worker.push.workDirs.allow` for a single repository. This condition is checked again on requests, including for roots that appear after startup. Git discovery errors refuse the root. Exact workdir entries must not contain a root or its ancestors: such a mount would let one session replace another session's checkout directory. Roots must remain host-owned.

Every path is resolved to its canonical form (symlinks resolved, trailing slashes stripped) via `std::fs::canonicalize`. Roots are re-resolved on every request, so a root created after startup works. If a root does not exist at startup, a warning is logged and it remains configured for later requests. Exact workdir entries must already exist. Background startup preserves absolute paths and the invoking working directory, including when relative entries are read again from global config.

To see the current allowlist over HTTP:

```sh
curl https://localhost:9876/v1/worker/workdirs
```

```json
{
  "items": [
    { "path": "/home/user/my-project", "kind": "allow" },
    { "path": "/home/user/other-project", "kind": "allow" },
    { "path": "/srv/repos", "kind": "root" }
  ],
  "next_cursor": null
}
```

`kind` is `allow` for an exact working directory and `root` for a directory that contains repositories.

---

## Job Queue

The worker feature uses a per-session FIFO queue to serialize command execution. When you submit a command via `POST /v1/worker/commands`, it is enqueued immediately and returns with a `command_id`. A pool of worker tasks processes the queue: each worker claims one queued command at a time and executes it, then claims the next one.

- **One command per session at a time**, in submission order, so commands in a session don't interfere with each other.
- **Concurrency across sessions**: the daemon shares a default capacity of eight concurrent jobs across enabled features. Set the shared cap with `daemon.maxConcurrentJobs`; see [Configuration](07-configuration.md#reference-all-keys).
- **Submission does not wait for execution**: a valid command for a ready session is enqueued with HTTP 202. `POST /v1/worker/commands` returns 409 if the session is closing or its setup is not ready; `POST /v1/worker/runs` can queue its command during setup.

Track progress with `GET /v1/worker/commands/{id}`, or inspect a session's queue depth, running command and recent completions with [`GET /v1/worker/sessions/{id}/queue`](#get-session-queue-status).

---

## HTTP API

All endpoints speak JSON. All requests and responses are logged at `INFO` level or above.

Network requests must include the key in the `Authorization: Bearer <key>` header. See [Authentication](#authentication) for details.

### Base URL

```
https://localhost:<port>/v1        # TLS-only network listener
```

The default TLS cert is self-signed (stored under `<data-home>/data/daemon/tls/`, where data home is `$AWMAN_HOME`, then `$XDG_DATA_HOME/awman`, then `~/.awman`), so HTTP clients will reject it unless you tell them not to. For local testing with curl:

```sh
# Easiest: skip verification (acceptable for loopback)
curl -k https://localhost:9876/v1/status -H "Authorization: Bearer $KEY"

# Stricter: pin the self-signed cert as the only trusted CA
curl --cacert ~/.awman/data/daemon/tls/cert.pem https://localhost:9876/v1/status \
  -H "Authorization: Bearer $KEY"
```

Substitute your data home in the certificate path when using `AWMAN_HOME` or `XDG_DATA_HOME`. Every `curl` example in the rest of this document elides `-k`/`--cacert` for brevity; add one of the two flags above to any of them when running against the default HTTPS server.

### Route tree

This is the complete route table. Request and response details for the worker push routes follow in the sections below. The pull management routes are local-only and are not exposed on the network listener.

| Method | Path | Feature | Access | Purpose |
|--------|------|---------|--------|---------|
| `GET` | `/v1/health` | core | unauthenticated | Health check |
| `GET` | `/v1/keys` | core | owner/local | List named API keys |
| `POST` | `/v1/keys` | core | owner/local | Create a key, shown once |
| `DELETE` | `/v1/keys/{id}` | core | owner/local | Revoke a key |
| `PATCH` | `/v1/features/{feature}` | core | owner/local | Enable or disable an ephemeral feature |
| `GET` | `/v1/status` | core | feature | Daemon status, capabilities, feature states |
| `GET` | `/v1/terminals` | core | owner/local or feature | List visible terminal records |
| `GET` | `/v1/terminals/{id}` | core | owner/local or feature | Read a terminal record or open its WebSocket |
| `GET` | `/v1/config` | core | owner/local | Masked effective configuration and where each value comes from |
| `POST` | `/v1/config/reload` | core | owner/local | Validate config files and apply live settings |
| `GET` | `/v1/features` | core | feature | List feature states |
| `GET` | `/v1/features/{feature}` | core | feature | One feature's state |
| `GET` | `/v1/grants` | core | owner/local | List remembered prerequisite grants |
| `DELETE` | `/v1/grants/{id}` | core | owner/local | Delete a grant (`204`) |
| `GET` | `/v1/worker/workdirs` | worker | feature | Allowlisted working directories and roots |
| `GET` | `/v1/worker/sessions` | worker | feature | List sessions (`status`, `setup_status`, `label` filters) |
| `POST` | `/v1/worker/sessions` | worker | feature | Create a session |
| `GET` | `/v1/worker/sessions/{id}` | worker | feature | Session detail, including `setup_status` / `setup_error` |
| `DELETE` | `/v1/worker/sessions/{id}` | worker | feature | Close a session |
| `GET` | `/v1/worker/sessions/{id}/queue` | worker | feature | Queue depth, running command, recent completions |
| `GET` | `/v1/worker/commands` | worker | feature | List commands (`session_id`, `status`, `label` filters) |
| `POST` | `/v1/worker/commands` | worker | feature | Submit a command (enqueued) |
| `GET` | `/v1/worker/commands/{id}` | worker | feature | Command record and structured `result` |
| `POST` | `/v1/worker/commands/{id}/cancel` | worker | feature | Cancel a queued or running command |
| `GET` | `/v1/worker/commands/{id}/events` | worker | feature | Server-Sent Events stream; `?format=json` for a snapshot |
| `GET` | `/v1/worker/commands/{id}/workflow` | worker | feature | Workflow state |
| `GET` | `/v1/worker/commands/{id}/outputs` | worker | feature | List files the agent wrote under `/awman/output` |
| `GET` | `/v1/worker/commands/{id}/outputs/{*path}` | worker | feature | Download one output file |
| `POST` | `/v1/worker/runs` | worker | feature | Create a session and run a command in one request |
| `GET` | `/v1/worker/servers` | worker | owner/local | List configured work server status |
| `GET` | `/v1/worker/servers/{name}` | worker | owner/local | Read one configured work server status |
| `GET` | `/v1/worker/queue` | worker | owner/local | List eligible work server items; `limit` and `cursor` page the response |
| `POST` | `/v1/worker/queue/{item_id}/claim` | worker | owner/local | Claim an item and run it in the daemon |
| `POST` | `/v1/worker/leases/{id}/checkout` | worker | owner/local | Prepare a matching-origin checkout worktree for a foreground pick |
| `POST` | `/v1/worker/leases/{id}/events` | worker | owner/local | Forward foreground lease events |
| `POST` | `/v1/worker/leases/{id}/terminal` | worker | owner/local | Exchange foreground terminal frames with the lease holder |
| `POST` | `/v1/worker/leases/{id}/result` | worker | owner/local | Report the foreground lease result |
| `GET` | `/v1/worker/leases` | worker | owner/local | List recorded worker leases |
| `GET` | `/v1/worker/leases/{id}` | worker | owner/local | Read one recorded worker lease |
| `POST` | `/v1/worker/leases/{id}/release` | worker | owner/local | Release an active work server lease |
| `GET` | `/v1/squad/tasks` | squad | feature | List tasks (`?name=` filter) |
| `POST` | `/v1/squad/tasks` | squad | feature | Create a task |
| `GET` | `/v1/squad/tasks/{id}` | squad | feature | Task detail |
| `PATCH` | `/v1/squad/tasks/{id}` | squad | feature | Edit a task; `{"paused": true\|false}` pauses and resumes |
| `DELETE` | `/v1/squad/tasks/{id}` | squad | feature | Delete a task (`204`) |
| `GET` | `/v1/squad/tasks/{id}/workflow` | squad | feature | The task's workflow |
| `POST` | `/v1/squad/tasks/{id}/runs` | squad | feature | Trigger a run now |
| `GET` | `/v1/squad/tasks/{id}/runs` | squad | feature | Run history (`?status=`, `limit`, `cursor`) |
| `POST` | `/v1/squad/runs/{id}/cancel` | squad | feature | Cancel a run |
| `GET` | `/v1/squad/env` | squad | feature | Names and digests of the persisted environment (never values) |
| `PUT` | `/v1/squad/env` | squad | feature | Merge values into the persisted environment |
| `DELETE` | `/v1/squad/env` | squad | feature | Clear the persisted environment |
| `GET` | `/v1/engine/instances` | engine | owner/local | List engine instances (repeat `annotation=key=value` to filter) |
| `POST` | `/v1/engine/instances` | engine | owner/local | Reserve an instance for a TUI to attach to |
| `GET` | `/v1/engine/instances/{id}` | engine | owner/local | One instance with its frontends and tabs |
| `PATCH` | `/v1/engine/instances/{id}` | engine | owner/local | Merge annotations (an empty value removes a key) |
| `DELETE` | `/v1/engine/instances/{id}` | engine | owner/local | Quit an instance |
| `GET` | `/v1/engine/instances/{id}/tabs` | engine | owner/local | List an instance's tabs |
| `GET`, `PATCH` | `/v1/engine/instances/{id}/tabs/{tab}` | engine | owner/local | Read a tab, or merge its annotations |
| `GET` | `/v1/engine/instances/{id}/dialogs` | engine | owner/local | Dialogs waiting for an answer |
| `POST` | `/v1/engine/instances/{id}/dialogs/{dialog_id}/answer` | engine | owner/local | Answer a pending dialog (`409 dialog_answered` if someone already did) |
| `POST` | `/v1/engine/instances/{id}/ui_requests` | engine | owner/local | Ask the instance to open or close a tab, change focus, and so on |
| `GET` | `/v1/engine/instances/{id}/frontend` | engine | owner/local | WebSocket a TUI attaches through |
| `GET` | `/v1/engine/catalogue` | engine | owner/local or browser grant | Command catalogue projection (`ETag`/`If-None-Match`) |
| `GET` | `/v1/engine/prompts` | engine | owner/local or browser grant | Confirmation prompt wording for `?kind=…` |
| `GET` | `/v1/engine/launch_contexts` | engine | owner/local or browser grant | Registered workspaces (never environment values) |
| `POST` | `/v1/engine/launch_contexts` | engine | owner/local | Register the calling shell's directory and environment |
| `POST` | `/v1/engine/launch_contexts/{id}/refresh` | engine | owner/local | Refresh a workspace from its root |
| `DELETE` | `/v1/engine/launch_contexts/{id}` | engine | owner/local | Remove a workspace |
| `POST` | `/v1/browser-links` | engine | allowed browser origin, loopback listener | Request a `web-` link code |
| `POST` | `/v1/browser-links/{id}/poll` | engine | allowed browser origin + polling secret | Poll a pending link; approval returns the grant |
| `GET` | `/v1/browser-links?code=web-…` | engine | owner/local | Look up a pending link (`awman link web-<code>`) |
| `POST` | `/v1/browser-links/{id}/decision` | engine | owner/local | Approve or deny a pending link |
| `GET` | `/v1/browsers` | core | owner/local | List browser grants (`awman daemon api browsers.list`) |
| `DELETE` | `/v1/browsers/{id}` | core | owner/local | Revoke a grant and close its connections (`browsers.delete`) |
| `POST` | `/v1/browser-tickets` | engine | browser grant | One-use WebSocket upgrade ticket |

### Engine routes

The `/v1/engine/*` routes belong to the daemon's engine feature, which hosts [TUI instances](19-engine-and-frontends.md). The TUI attaches through the owner-only local socket or named pipe. The engine feature can be selected on a named network listener only when it binds to loopback. The hosted web frontend uses the loopback listener after the owner approves a browser link with `awman link web-<code>`. These routes are not reachable through `awman remote`. From the command line, use `awman daemon api instances.list`, `instances.get`, `dialogs.list`, `dialogs.answer` and `ui.request`. Routes for an engine that is off answer `503 feature_disabled`.

Squad task and run bodies use `snake_case` fields; a task's interval is `interval_ms`. The scheduler currently has one-second granularity, so `interval_ms` must be a multiple of 1000. Anything not in this table — including the old unprefixed routes listed under [Upgrading from earlier betas](#upgrading-from-earlier-betas) — answers `404 not_found`.

The route tree is served by the daemon worker, squad, and core features; a route for a feature the process does not run answers `503 feature_disabled`.

### Daemon terminals

`GET /v1/terminals` lists terminals visible to the caller, newest first. Use
`owner_id=cmd_…`, `run_…`, `tab_…`, `lease_…`, or `task_…` to filter by owner;
`task_` selects its active squad run. `limit` and `cursor` use the standard
list envelope. Finished records remain listed while their owner remains
available. `GET /v1/terminals/{id}` returns one record. A malformed ID returns
`404 terminal_not_found`; a well-formed terminal ID that is unknown or has
expired returns `410 terminal_gone`.

Terminal records include the owner, feature, source (phase and workflow step
when present), agent/model, attempt, dimensions, open/exited state, and exit
code. These routes are served, but the current daemon does not register its
PTYs with the terminal hub, so live daemon lists may be empty and attach may
find no terminal. The worker event stream remains available for headless
commands.

To view a terminal, upgrade `GET /v1/terminals/{id}` to WebSocket and negotiate
the required `awman.terminal.v1` subprotocol. Frames are JSON text; byte fields
use padded standard base64. The server sends `snapshot`, `output`, `resize`,
`exit`, `lagged` (followed by a fresh snapshot), or `error` frames. Clients
send `take_control`, `input`, `resize`, and `detached` frames. A read-only
viewer never receives input control or resizes the PTY. The first viewer to
take the input grant controls it until detach or disconnect; the terminal
returns to its configured default size then.

```json
{"type":"snapshot","cols":120,"rows":40,"screen":"G1s/MjVs…"}
{"type":"output","bytes":"aGk="}
{"type":"take_control"}
{"type":"input","bytes":"bHMNCg=="}
{"type":"resize","cols":100,"rows":30}
{"type":"detached","reason":"user"}
```

If another viewer holds the grant, the server sends an `error` frame with
`code: "control_held"`. A read-only viewer may still watch output; attempted
input or resize is refused with `input_not_allowed`. Input waiting for an
agent that is not reading is limited to 256 KiB; beyond that an `input` frame
is refused whole with `input_backpressure` (the holder keeps control and can
retry). Resize requests are limited to 1000 columns by 500 rows; larger
values are clamped.

Permissions are re-checked live. Turning off `daemon.terminal.remoteInput`
(or no longer serving the owner's feature on the network listener) releases
a network viewer's input grant at its next `input`, `resize`, or
`take_control`, which are then refused with `input_not_allowed`. A network
viewer whose terminal is no longer served is disconnected with close code
1008.

Local socket access follows the local owner's permissions. Network access
uses the same bearer-key authentication as worker command events. Network
input requires both `daemon.terminal.remoteInput=true`, the command's
`allow_remote_input` permission, and an `x-awman-actor` header naming the
viewer. Local viewers may request control unless they connect read-only.
Set `"allow_remote_input": true` when submitting a command through
`POST /v1/worker/commands` or the command part of `POST /v1/worker/runs` to opt in.
It defaults to `false`, is persisted with the command, and is returned on the
command record. Both this permission and `daemon.terminal.remoteInput` must
allow input. A read-only WebSocket uses `?read_only=true`; it can never take control.

Terminal lifecycle and viewer events are written to the owning command's
event stream when it has one, or the squad run's `events.log`. `terminal_opened` and `terminal_closed` carry
the terminal ID and opening details or exit code; `terminal_control` carries
the holder and annotations (an empty holder means control was released), and
`terminal_viewer_detached` carries the detach reason and annotations.
Keystrokes are never logged. Agent OSC 7501 reports appear as
`program_status` events with `state` (`idle`, `working`, `done`, `blocked`, or
`error`) and optional `kind`, `progress`, `msg`, and `app`. A status clear has
no event. The advertised `agent_status` capability indicates support for
these reports.

The typed `interaction` request field chooses `interactive` PTY mode or
`headless` piped mode for pushed worker jobs. It must agree with any
`--interactive` or `--headless` flag in `args`; disagreement returns HTTP 400
`invalid_request`. When the request's mode differs from
`daemon.features.worker.push.interaction`, the server returns HTTP 400
`interaction_override_refused` unless
`daemon.features.worker.push.allowOverride=true`. Asking for the configured
mode is always allowed, and a refused request is not queued.

### `awman daemon api`

`awman daemon api` calls these routes from the command line without writing curl.

```
awman daemon api <action> [key=value]... [--json]
awman daemon api --list
awman daemon api --raw <METHOD> </v1/path> [--data <json>|@file|-] [--json]
```

**Where it connects.** With no `--remote` and no `remote.default`, it talks to the daemon over its owner-only local socket or named pipe described by `<data>/daemon/server.json`; no API key is needed, and this is the only transport that reaches owner-only routes. With `--remote <target>` or a `remote.default`, it connects over HTTPS to that [remote target](#remote-client-configuration) with the target's API key.

**Output.** Without `--json` the response is printed as a small key/value summary; with `--json` the body is printed as returned.

**Actions.** `awman daemon api --list` prints this table and the route each action calls.

| Action | Arguments | Route |
|--------|-----------|-------|
| `status` | — | `GET /v1/status` |
| `config.get` | — | `GET /v1/config` |
| `config.reload` | — | `POST /v1/config/reload` |
| `features.list` | `limit=`, `cursor=` | `GET /v1/features` |
| `features.get` | `feature=` | `GET /v1/features/{feature}` |
| `grants.list` | `limit=`, `cursor=` | `GET /v1/grants` |
| `grants.delete` | `id=grant_…` | `DELETE /v1/grants/{id}` |
| `sessions.list` | `status=`, `setup_status=`, `label=`, `limit=`, `cursor=` | `GET /v1/worker/sessions` |
| `sessions.get` | `id=ses_…` | `GET /v1/worker/sessions/{id}` |
| `sessions.delete` | `id=ses_…` | `DELETE /v1/worker/sessions/{id}` |
| `commands.list` | `session_id=`, `status=`, `label=`, `limit=`, `cursor=` | `GET /v1/worker/commands` |
| `commands.get` | `id=cmd_…` | `GET /v1/worker/commands/{id}` |
| `commands.cancel` | `id=cmd_…` | `POST /v1/worker/commands/{id}/cancel` |
| `tasks.list` | `name=`, `limit=`, `cursor=` | `GET /v1/squad/tasks` |
| `tasks.get` | `id=task_…` | `GET /v1/squad/tasks/{id}` |
| `tasks.delete` | `id=task_…` | `DELETE /v1/squad/tasks/{id}` |
| `runs.list` | `id=task_…`, `status=`, `limit=`, `cursor=` | `GET /v1/squad/tasks/{id}/runs` |
| `runs.cancel` | `id=run_…` | `POST /v1/squad/runs/{id}/cancel` |
| `jobs.cancel` | `id=cmd_…` or `id=run_…` | the cancel route for that ID's prefix |

Creating sessions, commands and tasks stays with `awman remote` and `awman squad`.

**Compositions.** Some actions are repeated calls, and each is reproducible with `--raw`:

| Composition | What it does | Equivalent |
|-------------|--------------|------------|
| `jobs.cancel feature=worker` | Cancel every running command | `--raw GET '/v1/worker/commands?status=running'`, then `--raw POST /v1/worker/commands/<id>/cancel` for each item (follow `next_cursor`) |
| `jobs.cancel feature=squad` | Cancel every running squad run | `--raw GET /v1/squad/tasks`, then `--raw GET '/v1/squad/tasks/<id>/runs?status=running'` per task, then `--raw POST /v1/squad/runs/<run>/cancel` |
| `grants.delete all` | Delete every remembered grant | `--raw GET /v1/grants`, then `--raw DELETE /v1/grants/<id>` for each item |

`steps.approve` and `overview.open` appear in `--list` as reserved names for a later release. They are not available yet: running either reports an unknown action.

**Raw requests.** `--raw` sends the method and the full `/v1/…` path as given. The body comes from `--data` as literal JSON, `@file`, or `-` for stdin.

```sh
awman daemon api status --json
awman daemon api grants.list
awman daemon api grants.delete id=grant_0123456789abcdef0123456789abcdef
awman daemon api jobs.cancel id=cmd_e5f6a7b8e5f6a7b8e5f6a7b8e5f6a7b8
awman daemon api --raw GET '/v1/worker/commands?status=running&limit=10' --json
awman daemon api --raw PATCH /v1/squad/tasks/task_0123456789abcdef0123456789abcdef --data '{"paused": true}'
awman --remote build daemon api sessions.list status=active
```

### Agent commands

Commands submitted through `POST /v1/worker/commands` expose an agent snapshot and
management operations across the configured runtime, independent of the selected
session's working directory:

| Subcommand | API availability |
|------------|------------------|
| `agents status` | Returns a one-time status snapshot. `--watch` is not available through the API. |
| `agents list` | Lists agents. |
| `agents detail <id>` | Returns details for the selected agent. |
| `agents kill <id>` | Stops and removes the agent; include `--yes` because API requests cannot answer an interactive confirmation. |

For example, set `subcommand` to `agents status` or `agents kill` and pass
arguments such as the agent ID and `--yes` in the request's `args` array. The
result is the same JSON payload as the CLI's `--json` output, emitted as
one `stdout_line` event and stored in the command's `result.data`. The same
commands are documented in [Monitoring running agents](03-agent-sessions.md#monitoring-running-agents).

---

### Sessions

#### Create a session

There are two types of sessions:

**Local session** — bound to a working directory on the server's filesystem:

```sh
curl -sk -X POST https://localhost:9876/v1/worker/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"local","workdir":"/home/user/my-project"}'
```

**Remote session** — clones a git repository on the server and operates within the clone:

```sh
curl -sk -X POST https://localhost:9876/v1/worker/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"remote","repo_url":"https://github.com/org/repo","branch":"main"}'
```

`session_type` defaults to `local`. The API body uses `session_type` as its discriminator (`local` or `remote`); the `awman remote session start` CLI exposes the same choice as `--type`. Both types return HTTP 202 immediately with the new session ID:

```json
{ "session_id": "ses_a1b2c3d4e5f67890abcdef1234567890" }
```

Setup (cloning, for a remote session, then the same checks and image preparation as `awman ready`) continues in the background. Commands can be submitted once `GET /v1/worker/sessions/:id` reports `ready`; until then `POST /v1/worker/commands` returns HTTP 409 `session_not_ready`. [`POST /v1/worker/runs`](#one-shot-runs) queues a command without waiting.

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

For a remote session, the optional `git_auth` object supplies credentials for
clone, setup fetch, remote-ref lookup and host-side push-back:

```json
{
  "git_auth": { "kind": "token", "token": "<token>" }
}
```

`kind` is `token` or `basic`. A token may include an optional `username`
(default `x-access-token`); `basic` requires `username` and `password`. The
secret is non-empty and at most 8 KiB; a username is non-empty and at most 256
bytes. Values cannot contain CR, LF or NUL. Do not put credentials in
`repo_url`: URL user information, query strings and fragments are rejected.
Credentials are held in daemon memory only. A daemon restart loses them; a
later operation on that session fails with `credentials_lost`, so create a new
session and resubmit the credential. Session credentials are supported only
for HTTPS repositories and loopback HTTP repositories, and only on platforms
where `/v1/status` advertises `session_git_auth`. Other URLs are refused with
`git_auth_unsupported_url`; unsupported platforms return
`git_auth_unsupported_platform`. Invalid shapes return `invalid_git_auth`.
An authentication failure during the synchronous merge-ref lookup returns
HTTP 422 `git_auth_failed`; during asynchronous setup or a command it appears
as `failure.stage: "git_auth"`, `failure.code: "git_auth_failed"`. If a
credentialed session is used after its in-memory credential has been lost,
admission fails fast with `credentials_lost` (HTTP 409) where possible; setup
or a queued command interrupted by restart reports the same structured failure.

`base_ref`, `merge_refs` and `push` are only valid on remote sessions (a local session returns HTTP 400), and each requires `branch` (HTTP 400 otherwise). A `branch` or `merge_refs` entry that is not a valid git branch name, or a malformed `base_ref`, is also rejected with HTTP 400. Merge targets reject `HEAD`, hexadecimal commit spellings (including abbreviated SHAs), and names advertised only as tags. A name that exists as both a branch and a tag selects the branch. An entirely missing ref fails during setup.

**Setup stages.** `GET /v1/worker/sessions/:id` reports the stage the session is in: `initializing`, `cloning_repository`, `setting_up_branch`, `merging_refs`, `running_ready`, then `ready` (or `failed`). The `merging_refs` stage runs only when `merge_refs` is non-empty; each ref is merged as `origin/<ref>` in order, and progress is reported per ref. If a ref does not exist on `origin`, or a merge conflicts, setup fails, the error names the stage (for a conflict it lists the conflicting paths), and the clone is deleted. Merge commits use your git identity (`user.name` / `user.email`) when configured, and a built-in awman identity otherwise.

**Pushing results.** With `push: true`, after an `exec prompt` or `exec workflow` command (including dynamic workflows) exits successfully, awman commits any leftover changes and pushes `HEAD` to `refs/heads/<branch>` on `origin`, setting an upstream and never forcing. Staging follows the repository's Git ignore rules. Workflow state JSON and saved dynamic TOML files can enter the commit when they are tracked or are not ignored. Other repository files, including Dockerfiles generated during setup, are eligible leftovers. A fresh branch with no new work is not published; setup merge commits are published even if the command makes no edits. If `origin` already has the same commit, no second push is needed. A successful publication, including a workflow's own push during this command, is recorded as `pushed: {"remote": "origin", "branch": "...", "sha": "..."}`; a no-op reports `pushed: null`. Failed, aborted or paused commands are not automatically pushed.

Commands share the clone's working state. Edits left by a failed command remain available for repair and are included if a later successful command leaves them in the clone. Workflow teardown runs before the automatic leftover commit: use `commit_changes` then `push_branch` before `create_pull_request` when the PR must already include those edits. A later automatic push updates the same branch.

Push-back retries temporary failures where Git received no response from the
remote. Authentication failures and remote rejections (such as a protected
branch, hook refusal or non-fast-forward update) are not retried. A response
from the remote, even an error response, ends the attempt. Before retrying,
awman checks whether `origin` already points at the commit being published; if
so, it treats publication as successful without pushing again. Cancellation
during a retry wait ends with `cancelled`. The retries are bounded and do not
force-push.

The structured `result.failure` distinguishes these outcomes:

| `stage` | `code` | Meaning |
|---------|--------|---------|
| `git_auth` | `git_auth_failed` | Git credentials were refused or unavailable during a Git operation. |
| `push` | `push_unreachable` | Push-back could not get a remote response before retrying was exhausted. |
| `push` | `push_failed` | The remote rejected the push or another non-retryable push error occurred. |
| current operation | `cancelled` | The command was cancelled, including during retry wait. |
| `git_auth` | `credentials_lost` | The session had `git_auth`, but a daemon restart cleared its in-memory credential. |

Each scheduled retry also emits a `push_retry` event with `attempt`,
`max_attempts`, `operation` (`push` or `ls-remote`), `delay_ms` and a safe
`remote` identifier. It contains no Git output or credentials. Clients can
detect these result codes and events through the `push_retry_codes` and
`push_retry_events` capabilities; `git_auth_failure` identifies support for
the structured Git authentication failure code.

After a failure, local commits remain in an open session's clone. A run with
`close_on_finish: true` closes the session and deletes its clone after retries
and final result recording, including on failure. Leave `close_on_finish` false
when you need to recover unpublished commits. Correct remote branch policy
and submit another successful command to publish the remaining changes. If
session credentials have expired or were lost, preserve the commits before
opening a new credentialed session. If the command was cancelled or the connection was lost,
inspect the remote branch first: a push may have landed before awman observed
the interruption. An already-published matching commit is recognized on the
next push attempt.

The push runs on the host as the user running the daemon. Without `git_auth`,
Git uses that user's global configuration and credentials, including SSH keys
and global credential helpers. With `git_auth`, awman uses the supplied
credential without falling back to global authentication files. Make sure the
selected credential can access `repo_url`. Host push-back uses isolated Git
metadata with hooks and repository-configured helpers disabled; the
destination comes from the session's saved URL. System Git configuration is
not read.

**Local sessions** bind to an existing directory on the server. The directory must be in the allowlist (a `daemon.features.worker.push.workDirs.allow` entry, or a Git repository directly beneath a `daemon.features.worker.push.workDirs.roots` root). Commands run against that directory; an `exec workflow` with `--worktree` (implied by the API's default `--yolo`) runs in a worktree of it under `~/.awman/worktrees/`.

**Remote sessions** clone a git repository into `<API root>/sessions/<session_id>/<repository-folder>` on the server. The API root is `api/` under the awman home (`$AWMAN_HOME`, else `$XDG_DATA_HOME/awman`, else `~/.awman`). Commands run inside the clone. Closing the session deletes the clone; command logs remain in API storage. This is useful for running ephemeral CI jobs without a local checkout.

#### End-to-end example: remote session with setup and teardown

The server clones the repository when the session is created; the workflow's setup installs dependencies, its steps implement the feature, and teardown tests, commits, pushes and opens a pull request.

**1. Create a remote session and wait for `ready`:**

```sh
SESSION=$(curl -sk -X POST https://localhost:9876/v1/worker/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"remote","repo_url":"https://github.com/org/my-project","branch":"main"}' \
  | jq -r .session_id)

until [ "$(curl -sk https://localhost:9876/v1/worker/sessions/$SESSION \
  -H 'Authorization: Bearer <api-key>' | jq -r .setup_status)" = ready ]; do sleep 5; done
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
CMD=$(curl -sk -X POST https://localhost:9876/v1/worker/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec workflow","args":["aspec/workflows/implement-feature.toml","--work-item","0053"]}' \
  | jq -r .command_id)

curl -Nsk https://localhost:9876/v1/worker/commands/$CMD/events -H 'Authorization: Bearer <api-key>'
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
curl -sk https://localhost:9876/v1/worker/sessions \
  -H 'Authorization: Bearer <api-key>'

# Active sessions only
curl -sk "https://localhost:9876/v1/worker/sessions?status=active" \
  -H 'Authorization: Bearer <api-key>'
```

The optional `status` query parameter filters by session status. Accepted values: `active`, `closing`, `closed`. The repeatable `label` parameter filters by [label](#labels) (`label=k=v` or `label=k`; all must match). The optional `setup_status` parameter filters by setup stage (`initializing`, `cloning_repository`, `setting_up_branch`, `merging_refs`, `running_ready`, `ready` or `failed`) and combines with `status` and `label`; for example `setup_status=failed` finds remote sessions whose clone or merge failed. An invalid value returns HTTP 400 `invalid_request`. Without filters, all sessions are returned a page at a time (see [List responses](#api-conventions)).

Every session record carries `setup_status`. When it is `failed`, the record also carries `setup_error: {"stage": "...", "message": "..."}` (omitted if no error was recorded).

The same readiness fields appear on `GET /v1/worker/sessions/:id` and all JSON responses of `DELETE /v1/worker/sessions/:id`. DELETE returns the session record when closed; while draining it returns a closing acknowledgement (202 initially, 200 if already closing) with `session_id`, `status`, readiness, `running_command_id`, `cancelled_count` and `message`.

```json
{
  "items": [
    {
      "id": "ses_a1b2c3d4…",
      "workdir": "/home/user/my-project",
      "status": "active",
      "created_at": "2026-04-20T12:00:00Z",
      "closed_at": null,
      "labels": { "ci/run": "4812" },
      "setup_status": "ready"
    },
    {
      "id": "ses_b2c3d4e5…",
      "workdir": "/home/user/my-project",
      "status": "closed",
      "created_at": "2026-04-19T09:00:00Z",
      "closed_at": "2026-04-19T09:47:13Z",
      "labels": {},
      "setup_status": "ready"
    }
  ],
  "next_cursor": null
}
```

#### Get session detail

```sh
curl -sk https://localhost:9876/v1/worker/sessions/<session-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the session record, including its `labels`. It also includes `setup_status` (and `setup_error` when setup failed), so a client can tell whether the session is ready for commands without a separate status request. For remote sessions this includes `repo_url`, `branch`, `base_ref`, `merge_refs` (omitted when empty) and `push`. Returns HTTP 404 if the ID does not exist.

#### Close a session

```sh
curl -sk -X DELETE https://localhost:9876/v1/worker/sessions/<session-id> \
  -H 'Authorization: Bearer <api-key>'
```

Gracefully closes a session. All queued commands are cancelled immediately, but any currently running command is allowed to finish before the session is marked closed.

**Behavior:**
- **No queued commands:** Session transitions immediately to `closed` status. Returns HTTP 200.
- **Queued commands, no running command:** All queued commands are cancelled. Session transitions immediately to `closed` status. Returns HTTP 200.
- **Running command:** The session transitions to `closing` status, rejecting new submissions. The running command is allowed to finish. Returns HTTP 202 with details:
  ```json
  {
    "session_id": "ses_a1b2c3d4…",
    "status": "closing",
    "running_command_id": "cmd_456…",
    "cancelled_count": 3,
    "message": "Session is closing. Waiting for running command to complete. Poll GET /v1/worker/sessions/{id} to monitor."
  }
  ```
  Poll `GET /v1/worker/sessions/{id}` to observe the transition to `closed`.

Closed sessions cannot receive new commands. All command records and output files are preserved — no data is deleted. Returns HTTP 404 if the session does not exist.

---

### Commands

Commands are submitted to a session and execute asynchronously. Submit a command and receive a `command_id` immediately; poll the command endpoint to track progress and retrieve output.

#### Submit a command

```sh
curl -sk -X POST https://localhost:9876/v1/worker/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"<session-id>","subcommand":"exec prompt","args":["hello"]}'
```

Dispatches a subcommand to the session named by the `session_id` field in the JSON body. The `x-awman-session` header is still supported as an alternative; if both are sent they must name the same session, otherwise the request is rejected with HTTP 400 `invalid_request`. Idempotency comparison covers whichever form was sent.

The API accepts these commands: `exec prompt`, `exec workflow`, `agents status|list|detail|kill` (see [Agent commands](#agent-commands)), and the `squad` task and daemon commands except `squad attach` (which needs an interactive PTY) and `squad env`. Anything else — `chat`, `ready`, `init`, `config`, `new`, `clean`, `remote` — is refused with HTTP 400 `invalid_request` ("command not available via API"), and the response lists the available commands. The [Command Reference](14-command-reference.md) marks each command's frontends.

For `exec`, put the action in `subcommand` (`exec prompt` or `exec workflow`) and the action's arguments in `args`. A bare `"subcommand": "exec"` with the action as the first `args` entry is rejected at admission with HTTP 400 `invalid_request`:

```sh
# exec prompt via API
curl -sk -X POST https://localhost:9876/v1/worker/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"<session-id>","subcommand":"exec prompt","args":["Fix the failing tests"]}'

# exec workflow via API
curl -sk -X POST https://localhost:9876/v1/worker/commands \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"<session-id>","subcommand":"exec workflow","args":["./aspec/workflows/implement-feature.toml","--work-item","0053","--prerequisite-unresolved","warn"]}'
```

Workflow prerequisite options are passed in the same `args` array as CLI
flags: `--prerequisite-identity`, `--prerequisite-ssh`,
`--prerequisite-token`, and `--prerequisite-unresolved`. Use
`--check-prerequisites` to return the prerequisite report without starting the
workflow. API commands run headless by default: flags can grant a specific choice,
but the API caller cannot silently widen the configured credential policy.
Set global `workflows.prerequisites.allowApiClientGrants` to `true` only if API
clients should be allowed to grant credentials beyond the normal policy.

**Workflow files:** For `exec workflow`, the workflow file path is specified as a positional argument relative to the session's working directory (same format as the CLI). The file must exist within the session's working directory and be a valid TOML or YAML workflow definition. Workflow files are never passed as inline JSON — always use file paths.

Invalid prerequisite syntax and unsupported `secret:NAME` return HTTP 400 at admission. Credential-widening requests return HTTP 403 unless globally authorized. Accepted responses include the secret-free resolved policy in `flags_applied.prerequisites`.

Optional body field `interaction` (`"headless"` or `"interactive"`) is the
typed way to choose the mode. When omitted, the server uses
`daemon.features.worker.push.interaction` (default `headless`). For
`exec prompt` and `exec workflow`, `--headless`/`-n` and `--interactive` in
`args` request the same choice; if a body field and flag disagree, admission
returns HTTP 400 `invalid_request`. A request that differs from the configured
mode is refused with HTTP 400 `interaction_override_refused` unless
`daemon.features.worker.push.allowOverride` is `true` (the default). Refused
requests are not enqueued.

Returns immediately with a command ID — the command is **enqueued** and execution is asynchronous:

```json
{
  "command_id": "cmd_e5f6a7b8…",
  "flags_applied": { "yolo": true, "headless": true }
}
```

The optional `secrets` field supplies values for command overlays that request
`env(NAME)`:

```json
{
  "session_id": "<session-id>",
  "subcommand": "exec prompt",
  "args": ["hello"],
  "secrets": { "DEPLOY_TOKEN": "<value>" }
}
```

It is also accepted as `command.secrets` by `POST /v1/worker/runs`. A command
may supply up to 32 secrets. Each value must be a non-empty string of at most
8192 bytes without NUL. Names are at most 128 bytes and must match
`^[A-Z_][A-Z0-9_]*$`; duplicate names are rejected. These values override the
host environment for the matching `env(NAME)` overlay. The overlay must
explicitly request each variable; command secrets are not added to every
container environment.

Secret values exist in daemon memory for the command lifetime and are not
written to the command record or idempotency record. After a daemon restart,
queued commands whose secret values were lost fail before execution with
`failure.stage: "preflight"` and `failure.code: "secrets_lost"`; submit the
command again with its secrets. Events, command results and logs are redacted
for registered secret values and common encoded forms. Redaction is not
content scanning for files: `/awman/output` files and raw terminal bytes are
not redacted and must not be used to store or display secrets.

The command begins executing as soon as a worker claims it from the queue. **At most one command per session runs at any given time** — commands within a session are processed sequentially in FIFO order. If a command is already running, subsequent commands are queued and wait their turn (they do not block the API request).

**`flags_applied`** reports the API's flag defaults for the submitted command. It lists only flags the command accepts, so it is `{}` for commands such as `agents list`:

| Flag | Behavior |
|------|----------|
| `headless` | `true` unless the request asks for `"interaction": "interactive"` (see below). API workers have no terminal, so by default `exec prompt` and `exec workflow` run in print/batch mode. You never need to pass `--headless`. |
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
curl -sk https://localhost:9876/v1/worker/commands/<command-id> \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current status and metadata for a command, including its source `listener` and optional `trace_parent`. Worker create routes accept a W3C version 00 `traceparent` header; malformed values are ignored. A traced command stream starts with an id-less `queue_status` event carrying that trace and listener. This event leaves the resumable execution-event cursor unchanged.

```json
{
  "id": "cmd_e5f6a7b8…",
  "session_id": "ses_a1b2c3d4…",
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
| `cancelled` | Cancelled by [`POST /v1/worker/commands/:id/cancel`](#cancel-a-command), or before execution by a session kill. Exit code 130 when it had started |

**`log_path` (deprecated).** Responses to requests from a loopback address (`127.0.0.0/8` or `::1`) also carry `log_path`, the path of the command's output log on the server. It is omitted when the TCP peer is not loopback, and is deprecated: read output through `/v1/worker/commands/{id}/events` and `/v1/worker/commands/{id}/outputs`. The address checked is the immediate TCP peer; forwarding headers are ignored. SSH tunnels and proxies running on the API host connect from loopback, so their clients can also receive this field. This check does not establish that the original client is local.

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
`/v1/worker/runs` setup failure (`setup`), and is absent for every other failure and
for cancellations, whereas `failure.stage` is always present; `setup_error`
holds the `/v1/worker/runs` setup details. The legacy
`error` field remains available as the human-readable message — except for a
`/v1/worker/runs` command whose session setup failed: its `error` is the
literal string `"setup_error"`, not a message. The details are in
`result.setup_error` (`{ "stage", "message" }`, or `null`) and in the structured
`result.failure` (`stage: "setup"`, `code: "setup_failed"`, with the setup
message). Read `failure` rather than matching on `error`. A command row
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
| `setup_failed` | `/v1/worker/runs` session setup, or an aborting workflow setup step, failed |
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

**Malformed `args`:** `POST /v1/worker/commands` and the nested command in `POST /v1/worker/runs` validate flags and positional arguments against the command catalogue before enqueueing. Unknown flags or invalid typed values return HTTP 400 without creating a command, for example:

```json
{
  "error": "unknown flag 'not-a-real-flag' for command [\"exec\", \"prompt\"]",
  "code": "unknown_flag"
}
```

Execution failures after admission are recorded in the command's `result`; poll `GET /v1/worker/commands/{id}` or follow its log stream.

#### Get a command log snapshot

```sh
curl -sk 'https://localhost:9876/v1/worker/commands/<command-id>/events?format=json' \
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
      "emitted_at": "…",
      "attempt": 1,
      "sequence": 0,
      "payload": { "type": "stdout_line", "data": "…" }
    }
  ]
}
```

The default form of this endpoint is the live SSE stream described below.

#### Stream command logs (live)

```sh
curl -sk https://localhost:9876/v1/worker/commands/<command-id>/events \
  -H 'Authorization: Bearer <api-key>'
```

Opens a persistent HTTP response using [Server-Sent Events (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events). The server replays persisted events, then sends new events as they arrive. A terminal `event: done` frame means the command really ended: the command status is `done`, `error`, or `cancelled`. The server sends it only after the terminal status is recorded, then closes the response. It does not send `event: timeout`; a worker wait or a quiet command does not mean the command ended.

**SSE event format:**

```
id: 1:0
event: stdout_line
data: {"emitted_at":"…","attempt":1,"sequence":0,"payload":{"type":"stdout_line","data":"<line of output>"}}

id: 1:1
event: stderr_line
data: {"emitted_at":"…","attempt":1,"sequence":1,"payload":{"type":"stderr_line","data":"<line of output>"}}

id: 1:2
event: done
data: {"emitted_at":"…","attempt":1,"sequence":2,"payload":{"type":"done"}}

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
curl -sk https://localhost:9876/v1/worker/commands/<command-id>/events \
  -H 'Authorization: Bearer <api-key>' \
| while IFS= read -r line; do
    case "$line" in
      "event: done") echo "--- done ---"; break ;;
      data:\ *)              echo "${line#data: }" ;;
    esac
  done
```

**Behaviour notes:**

- Agents that report OSC 7501 status produce a `program_status` SSE event with
  the latest `state` and optional `kind`, `progress`, `msg`, and `app`. Events
  are sent when the effective status changes; clearing a report produces no
  status event. The event's `source` identifies its workflow step when
  available. These events are also persisted with the command's event log.
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
curl -Nsk -H 'Authorization: Bearer <api-key>' \
  -H 'Last-Event-ID: 2:17' \
  'https://localhost:9876/v1/worker/commands/<command-id>/events'

# Equivalent query form (takes precedence if the header is also present)
curl -Nsk -H 'Authorization: Bearer <api-key>' \
  'https://localhost:9876/v1/worker/commands/<command-id>/events?after=2:17'
```

`awman remote exec --follow` uses this endpoint internally. The cURL form above is equivalent and is useful in scripts where the awman binary is unavailable on the client.

Add `?step=<step-name>` to the live logs URL or JSON snapshot URL to receive only events attributed
to that workflow step. Command-level completion events (`command_status` and
`done`) are still sent so the filtered stream can finish on its own. For
example:

```sh
curl -Nsk 'https://localhost:9876/v1/worker/commands/<command-id>/events?step=implement' \
  -H 'Authorization: Bearer <api-key>'
```

**Structured events:** each event frame carries a named `event:` type (`stdout_line`,
`stderr_line`, `status_message`, `workflow_prerequisites`, `workflow_step_transition`,
`workflow_phase_transition`, `workflow_parallel_step_launched`,
`workflow_parallel_step_exited`, `workflow_parallel_group_finished`,
`command_status`, `step_activity`, `done`) and a JSON `data:` payload, so a client that wants
structured data instead of log lines can read the `event:` field and parse
accordingly. Add `?format=json` to the same URL to fetch the event-log snapshot
described above. It includes all retained attempts and their historical `done`
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
payload is `{ "type": "StepActivity", "data": { "kind": "message" |
"tool_use" | "tool_result" | "thinking", "summary": "…" } }` — `kind` and
`summary` sit inside `data`, like every other event payload. The summary is a bounded
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
curl -sk https://localhost:9876/v1/worker/sessions/<session-id>/queue \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current queue state for a session — how many commands are queued, which one is running, and recently completed commands.

```json
{
  "session_id": "ses_a1b2c3d4…",
  "queue_depth": 3,
  "running": {
    "command_id": "cmd_456…",
    "subcommand": "exec workflow",
    "args": ["deploy.toml"],
    "started_at": "2026-04-20T12:05:00Z",
    "worker_id": "worker_789"
  },
  "queued": [
    {
      "command_id": "cmd_457…",
      "subcommand": "exec workflow",
      "args": ["test.toml"],
      "queued_at": "2026-04-20T12:01:00Z",
      "position": 0
    },
    {
      "command_id": "cmd_458…",
      "subcommand": "exec prompt",
      "args": ["review the code"],
      "queued_at": "2026-04-20T12:02:00Z",
      "position": 1
    }
  ],
  "recent_completed": [
    {
      "command_id": "cmd_455…",
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
| `session_id` | The session ID |
| `queue_depth` | Total number of enqueued (not yet running) commands |
| `running` | The currently executing command, or `null` if none |
| `queued` | Array of enqueued commands, in FIFO order, with 0-indexed position |
| `recent_completed` | Array of the 10 most recent completed commands (status `done`, `error` or `cancelled`), newest first |

The `running`, `queued` and `recent_completed` entries each include a `labels` object.

This endpoint is useful for monitoring queue depth and diagnosing stalls in multi-command workflows.

#### Cancel a command

```sh
curl -sk -X POST https://localhost:9876/v1/worker/commands/<command-id>/cancel \
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
  "command_id": "cmd_e5f6a7b8…",
  "session_id": "ses_a1b2c3d4…",
  "status": "running",
  "cancel_requested_at": "2026-04-20T12:06:00Z",
  "message": "Stopping. Poll GET /v1/worker/commands/{id} until status is cancelled."
}
```

Poll `GET /v1/worker/commands/<command-id>` until `status` is `cancelled`. Repeating the cancel while the command is still stopping returns `202` again and keeps the original `cancel_requested_at`. A command that finishes cleanly before the stop takes effect stays `done`; the cancel does not rewrite a completed result.

A cancelled command has `status: "cancelled"` and `exit_code: 130`. Its `result` has `outcome: "cancelled"`, `failure.code: "cancelled"` (with the stage it was cancelled in) and the legacy `error: "cancelled"`. A command cancelled while running also carries `cancelled_at`.

**Cancellation is not a failure.** A workflow's `on_failure` retry and remediation handling is not triggered, and `abort_on_failure` is not applied. For a workflow command that is cancelled partway through:

- Teardown still runs, so **cleanup steps are kept**.
- Teardown **skips the publishing steps**: `push_branch`, `create_pull_request` and `poll_ci`. For a session created with `push: true`, the automatic push of the session branch is skipped too.
- A cancel that arrives during teardown or push-back still ends the command `cancelled`. Publishing steps that have not started are skipped; a push already in progress cannot be undone. A command that finished before the cancellation request keeps its result.

After a daemon restart, a recorded cancellation stays cancelled. The daemon stops surviving command-owned agents and finishes interrupted cleanup before starting queue workers or deleting a one-shot run's clone. Completed cleanup entries are retained; a cleanup entry interrupted by the crash may run again, so cleanup commands should be idempotent. Setup, main steps and remediation are not relaunched during this recovery.

`exec prompt` (including ACP agents), `exec workflow` and dynamic workflows are all cancellable. Closing a session with `DELETE /v1/worker/sessions/:id` still lets a running command finish; use the cancel endpoint first if you want it stopped.

#### List and retrieve command outputs

API-backed agent runs can write files under `/awman/output`. List the files
for a command with `GET /v1/worker/commands/{id}/outputs`; fetch one with
`GET /v1/worker/commands/{id}/outputs/{path}`:

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
curl -sk https://localhost:9876/v1/worker/commands/<command-id>/outputs \
  -H 'Authorization: Bearer <api-key>'

curl -sk https://localhost:9876/v1/worker/commands/<command-id>/outputs/report.json \
  -H 'Authorization: Bearer <api-key>'
```

The listing is a [list response](#api-conventions): `items` holds entries with a relative `path`, `size_bytes`, `sha256` and `modified_at`. The command's totals (`{ "files", "bytes" }`) appear in `result.outputs`. A missing output directory is an empty
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
curl -sk "https://localhost:9876/v1/worker/commands?label=ci/job=nightly&status=running" \
  -H 'Authorization: Bearer <api-key>'
```

| Query parameter | Meaning |
|-----------------|---------|
| `session_id` | Only commands in this session |
| `status` | One of `queued`, `running`, `done`, `error`, `cancelled` |
| `label` | A [label](#labels) selector, `k=v` or `k`. Repeat the parameter to require several; all must match |
| `limit` | Page size; default 50, maximum 500 (see [List responses](#api-conventions)) |
| `cursor` | The `next_cursor` from the previous page |

Returns `{"items":[...],"next_cursor":…}`, newest first. Each entry has the same shape as `GET /v1/worker/commands/:id`. An unknown `status` or malformed selector returns HTTP 400.

---

### Error responses

Idempotent replays preserve the originally stored response bytes. Errors cached by a server version before the `error_codes` feature may lack `code` until the key expires (24 hours).

New error responses are JSON with a human-readable `error` message and a stable, machine-readable `code`:

```json
{ "error": "Session 'ses_a1b2c3d4…' not found", "code": "session_not_found" }
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
| `origin_not_allowed` | 403 | A browser `Origin` that is not in the receiving listener's `allowedOrigins` (no CORS headers are sent) |
| `host_mismatch` | 421 | A plain-HTTP loopback request whose `Host` is not exactly the listener's configured address |
| `browser_grant_required` | 401 | A browser request without an approved grant |
| `browser_grant_invalid` | 401 | The grant is unknown, expired, revoked, or bound to another origin or listener |
| `browser_ticket_invalid` | 401 | A WebSocket upgrade without a valid one-use ticket, or a ticket on a non-upgrade request |
| `link_gone` | 410 | A browser link that is unknown, consumed or expired, or a poll with the wrong secret |
| `link_not_found` | 404 | `awman link web-<code>` named an unknown, expired or already decided code |
| `rate_limited` | 429 | Too many link creations, poll failures or lookups; see `Retry-After` |
| `feature_disabled` | 503 | The route belongs to a [feature](#api-conventions) this daemon is not running |
| `config_invalid` | 422 | `POST /v1/config/reload` found a config file that fails validation; the body also carries `errors: [string]` |
| `task_not_found` / `run_not_found` / `grant_not_found` | 404 | No squad task, squad run or grant with that ID |
| `task_exists` | 409 | `POST /v1/squad/tasks` with a name already in use |
| `prerequisite_widening_refused` | 403 | A squad task or worker command asked for broader credential access than policy allows |
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
| `terminal_not_found` | 404 | The terminal ID is malformed or does not name an available terminal |
| `terminal_gone` | 410 | A well-formed terminal ID is unknown, forgotten, or expired |
| `control_held` | 409 | Another viewer currently holds terminal input control |
| `input_not_allowed` | 403 | Terminal input is not allowed by the listener or command settings |
| `input_backpressure` | 429 | The agent is not reading terminal input and its input queue is full; retry later |
| `interaction_override_refused` | 400 | The requested interaction mode differs from daemon policy and overrides are disabled |

`unknown_subcommand`, `unknown_flag`, `forbidden_workdir` and the last six codes are the same values reported asynchronously in a command's `result.failure.code`, so one decoder serves both. Several of them can also be returned synchronously when a request is rejected at admission. `invalid_request`, `forbidden` and `not_found` are HTTP class codes only: a command that fails after admission reports the specific condition instead (see [Failure codes](#get-command-status)). A run whose setup actually fails uses `setup_failed`; submitting a new command to an unready session uses `session_not_ready`, because that command has not been admitted.

---

### Labels

Sessions and commands carry optional string labels, set at creation and returned on every read. Use them to tag work with your own identifiers (a job name, a pull request number) and to find it again later.

```sh
curl -sk -X POST https://localhost:9876/v1/worker/sessions \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{"session_type":"remote","repo_url":"https://github.com/org/repo","branch":"main",
       "labels":{"ci/run":"4812","team/name":"payments"}}'
```

`POST /v1/worker/sessions`, `POST /v1/worker/commands` and `POST /v1/worker/runs` accept `labels` as a JSON object of strings (default `{}`). Responses that describe a session or command — `GET` detail, status and list, `GET /v1/worker/sessions/:id/queue` entries, and the `DELETE` response — always include a `labels` object, empty when there are none.

**Limits.** At most 16 labels per object. Each key and value is at most 256 UTF-8 bytes. A key is `<namespace>/<name>`: exactly one `/`, both sides non-empty, using only lowercase `[a-z0-9.-]` (for example `ci/run` or `acme.io/job`). Keys under the `awman/` namespace are reserved and refused with `invalid_request`. Labels stored before this rule (such as a dotted `acme.kind`) stay readable and can still be used in `label=` filters; only new writes are validated. Neither keys nor values may contain control characters. Empty values are allowed. Anything else returns HTTP 400 with a validation message.

**Filtering.** `GET /v1/worker/sessions` and `GET /v1/worker/commands` accept a repeatable `label` query parameter:

- `label=team/name=payments` matches objects whose `team/name` label is exactly `payments`.
- `label=team/name` matches objects that have a `team/name` label with any value.
- Repeated selectors are ANDed.

`GET /v1/worker/sessions` also accepts `status=active|closing|closed`.

**Example: reattach after a restart.** A CI wrapper that tagged its work can recover after it crashes or is rescheduled, without having stored any IDs:

```sh
# Is there still a running command for this CI run?
curl -sk "https://localhost:9876/v1/worker/commands?label=ci/run=4812&status=running" \
  -H 'Authorization: Bearer <api-key>' | jq -r '.items[0].id'

# Or find the session, then its commands
curl -sk "https://localhost:9876/v1/worker/sessions?label=ci/run=4812&status=active" \
  -H 'Authorization: Bearer <api-key>' | jq -r '.items[0].id'
```

Once you have the command ID, resume with `GET /v1/worker/commands/<id>/events` or the status endpoint, or stop it with the cancel endpoint.

---

### Idempotency keys

Network failures leave a client unsure whether a create request arrived. `POST /v1/worker/sessions`, `POST /v1/worker/commands` and `POST /v1/worker/runs` accept an optional `Idempotency-Key` header so the request can be retried safely:

```sh
curl -sk -X POST https://localhost:9876/v1/worker/runs \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Idempotency-Key: ci-4812-attempt-1' \
  -H 'Content-Type: application/json' \
  -d @run.json
```

The key is 1–255 visible ASCII characters; anything else returns HTTP 400. Keys are remembered for 24 hours, per route and per API key (or shared when authentication is disabled). Two requests are the same when their JSON bodies are equivalent; for `POST /v1/worker/commands` the selected session is part of the comparison, whether it was sent as the body `session_id` or the `x-awman-session` header (the same command and session replays either way).

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

`POST /v1/worker/runs` creates a session and submits a command in a single request. It replaces the sequence create session → poll `GET /v1/worker/sessions/:id` until `ready` → `POST /v1/worker/commands` → `DELETE /v1/worker/sessions/:id`.

```sh
curl -sk -X POST https://localhost:9876/v1/worker/runs \
  -H 'Authorization: Bearer <api-key>' \
  -H 'Content-Type: application/json' \
  -d '{
    "session": {
      "session_type": "remote",
      "repo_url": "https://github.com/org/repo",
      "branch": "main",
      "labels": { "ci/run": "4812" }
    },
    "command": {
      "subcommand": "exec workflow",
      "args": ["./aspec/workflows/implement-feature.toml", "--work-item", "0053"],
      "labels": { "ci/run": "4812" }
    },
    "close_on_finish": true
  }'
```

Response (HTTP 202):

```json
{ "session_id": "ses_a1b2c3d4…", "command_id": "cmd_e5f6a7b8…" }
```

`session` takes the same fields as [Create a session](#create-a-session) and `command` the same fields as [Submit a command](#submit-a-command) except `session_id` (400 `invalid_request` if supplied: the run creates and selects its own session), and both are validated before anything is created (HTTP 400, 403 or 404 as for those routes). The command is queued immediately and starts on its own once session setup reaches `ready`, so there is no readiness check and no HTTP 409 for an unready session. `Idempotency-Key` is supported.

Track the run with `GET /v1/worker/commands/<command_id>` and `GET /v1/worker/commands/<command_id>/events`.

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
  "error": "setup_error",
  "setup_error": { "stage": "clone", "message": "..." }
}
```

The legacy `error` value for a setup failure is the literal string `"setup_error"` — it is not a message. The details are in `setup_error` (the setup stage and message) and in the structured `failure`; read those. Responses can also include the deprecated `error_status` and `stage` fields. Use `failure` in new clients.

**Cancelling during setup.** `POST /v1/worker/commands/<command_id>/cancel` works while setup is still running: setup is aborted, the command ends `cancelled`, and the session is closed and any remote clone deleted — whatever `close_on_finish` is set to.

---

### Server health

```sh
curl -sk https://localhost:9876/v1/status \
  -H 'Authorization: Bearer <api-key>'
```

The response shape is shown under [`GET /v1/status`](#get-v1status). `features` maps each feature to its state (`on`, `off`, `draining`, `ephemeral`, or `degraded`) with its `running` and `queued` job counts, and `max_concurrent_jobs` is the shared daemon capacity. `GET /v1/features` returns the same states as a list, and `GET /v1/features/worker` returns one.

The `capabilities` array lists what clients can detect without relying on the server version. `workflow_state` means the server exposes live workflow snapshots through `GET /v1/worker/commands/:id/workflow`.
`command_cancel`, `labels`, `idempotency` and `runs` mean the server supports [command cancellation](#cancel-a-command), [labels](#labels), [idempotency keys](#idempotency-keys) and [`POST /v1/worker/runs`](#one-shot-runs).
`error_codes` means every error body carries a `code` ([Error responses](#error-responses)). `structured_results` means command records carry the structured [`result`](#get-command-status); `outputs` means the [outputs endpoints](#list-and-retrieve-command-outputs) exist; `event_source` means events carry `source` and the event stream accepts `?step=`; `step_activity` means `step_activity` events can appear. `command_record` means `GET /v1/worker/commands/:id` is served. `session_setup_status` means sessions carry `setup_status` and `setup_error` and `GET /v1/worker/sessions` accepts `setup_status`. `command_session_body` means `POST /v1/worker/commands` accepts `session_id` in the body. `remote_dynamic` means `--dynamic` is accepted in `args`; `workdir_roots` means workdir roots are honoured; `event_ids` means SSE frames carry `id:` and `Last-Event-ID` resumes. `resource_routes`, `feature_states`, `config_routes`, `grant_routes`, `typed_ids`, `list_pages` and `headless_interaction` mean the [route tree](#route-tree), feature states, `/v1/config*`, `/v1/grants*`, prefixed IDs, list envelopes and the `interaction` request field.

New capabilities are added to `capabilities`, never removed, so check for a capability rather than comparing versions.

Git and credential capabilities describe current API behavior:
`git_auth_failure` advertises structured Git authentication failures;
`push_retry_events` advertises `push_retry` events;
`push_retry_codes` advertises `push_unreachable` and `cancelled` push failure
codes; `session_git_auth` advertises session credential input on supported
platforms; and `command_secrets` advertises command-scoped `secrets` delivery
and event/result redaction.

---

### Workflow state

API-mode `exec workflow` commands launch available steps automatically and
finish when the last step completes. A failed step is retried once, then fails
the command (`error`); a step with `abort_on_failure` aborts it. No interactive
confirmation is required.

When a workflow starts, the worker feature snapshots its `WorkflowState` to
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
curl -sk https://localhost:9876/v1/worker/commands/<command-id>/workflow \
  -H 'Authorization: Bearer <api-key>'
```

Returns the current workflow view for the given command. The response retains
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

Always also watch `GET /v1/worker/commands/{command_id}` or `command_status` events.
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
SERVER=https://localhost:9876
KEY=<your-api-key>

# Poll workflow progress until this command execution stops
while true; do
  RESP=$(curl -sk "$SERVER/v1/worker/commands/$CMD/workflow" \
    -H "Authorization: Bearer $KEY")
  PHASE=$(echo "$RESP" | jq -r '.phase // .current_phase')
  echo "Workflow phase: $PHASE"
  COMMAND=$(curl -kfsS "$SERVER/v1/worker/commands/$CMD" \
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
KEY=<your-api-key>   # created locally with awman daemon api keys.create
AUTH="Authorization: Bearer $KEY"

# 1. Create a session and wait until its setup is ready
SESSION=$(curl -sk -X POST "$SERVER/v1/worker/sessions" -H "$AUTH" \
  -H 'Content-Type: application/json' \
  -d '{"workdir":"/home/user/my-project"}' | jq -r .session_id)
until [ "$(curl -sk "$SERVER/v1/worker/sessions/$SESSION" -H "$AUTH" | jq -r .setup_status)" = ready ]; do
  sleep 2
done

# 2. Submit a command
CMD=$(curl -sk -X POST "$SERVER/v1/worker/commands" -H "$AUTH" \
  -H 'Content-Type: application/json' \
  -d '{"session_id":"'"$SESSION"'","subcommand":"exec workflow","args":["./aspec/workflows/implement-feature.toml"]}' \
  | jq -r .command_id)

# 3. Poll until terminal (queued -> running -> done | error | cancelled)
while true; do
  STATUS=$(curl -sk "$SERVER/v1/worker/commands/$CMD" -H "$AUTH" | jq -r .status)
  echo "Status: $STATUS"
  case "$STATUS" in done|error|cancelled) break ;; esac
  sleep 10
done

# 4. Print the captured output from the JSON event snapshot
curl -sk "$SERVER/v1/worker/commands/$CMD/events?format=json" -H "$AUTH" \
  | jq -r '.events[] | select(.payload.type == "stdout_line" or .payload.type == "stderr_line") | .payload.data'

# 5. Close the session
curl -sk -X DELETE "$SERVER/v1/worker/sessions/$SESSION" -H "$AUTH"
```

For a single task without a server, `awman exec prompt "…" --headless` runs locally and exits with the agent's exit code. Going through the API instead records the run durably and makes it visible to other clients.

---

## Storage layout

Worker session files live under `daemon/` in the awman home (`$AWMAN_HOME`, default `~/.awman`):

```
$AWMAN_HOME/daemon/
  awman.log                        # daemon log
  awman.pid                        # PID file for the running daemon
  api-keys.json                    # named key hashes; plaintext keys are never stored
  tls/cert.pem, tls/key.pem        # network-listener TLS certificate and key
  sessions/
    <ses_id>/
      setup_state.json             # session setup progress
      <repository-folder>/         # remote sessions only: the clone, deleted on close
      commands/
        <cmd_id>/
          output.log               # combined stdout+stderr (written incrementally)
          events.log               # structured events served by /events
          output/                  # files the agent wrote to /awman/output
          workflow.state.json      # workflow commands only
          completion.json          # recorded completion, used for restart recovery
          cleanup.json             # pending workflow cleanup; removed after completion
```

The shared SQLite database is stored separately at
`$AWMAN_HOME/data/awman.db`.

`workflow.state.json` is the snapshot served by [Get workflow state](#get-workflow-state). It is written atomically and uses the same JSON structure as the local workflow state in `$GITROOT/.awman/workflows/`.

`awman.db` holds worker session and command records (and squad task and run records): the session and command fields described in this guide, plus labels. It is the authoritative record of activity; the per-command files hold raw output. Neither is deleted when a session is closed (see [Session cleanup](#session-cleanup)).

---

## Daemon configuration

Worker and network settings live in the `daemon` section of global config (`$AWMAN_HOME/config.json`, default `~/.awman/config.json`). All fields are optional; see [Configuration](07-configuration.md#reference-all-keys) for the full reference.

```json
{
  "daemon": {
    "maxConcurrentJobs": 8,
    "verboseSetup": true,
    "network": {
      "listeners": {
        "default": {
          "enabled": true,
          "listen": "127.0.0.1:9876",
          "transport": "tls",
          "features": ["worker"]
        }
      }
    },
    "features": {
      "worker": {
        "push": {
          "interaction": "headless",
          "allowOverride": true,
          "workDirs": {
            "allow": ["/home/user/my-project", "/home/user/other-project"],
            "roots": ["/srv/repos"]
          }
        }
      }
    }
  }
}
```

| Key | Meaning |
|-----|---------|
| `daemon.maxConcurrentJobs` | Shared maximum number of daemon jobs running at once (default 8). Live: `awman config set` applies it to a running daemon. |
| `daemon.network.listeners.<name>.listen` | `host:port` the named listener binds. Restart required. |
| `daemon.verboseSetup` | Log full session-setup output at `info` level (default `false`). |
| `daemon.features.worker.push.workDirs.allow` | Exact working directories clients may use. |
| `daemon.features.worker.push.workDirs.roots` | Directories whose direct-child Git repositories clients may use. |
| `daemon.features.worker.push.interaction` | `headless` (default) or `interactive`: how API commands run when the request does not say. |
| `daemon.features.worker.push.allowOverride` | Whether a request's `interaction` may differ from the setting above (default `true`). |

```sh
awman config set --global daemon.features.worker.enabled true
awman config set --global daemon.features.worker.push.workDirs.allow "/home/user/my-project,/home/user/other-project"
awman config set --global daemon.features.worker.push.workDirs.roots "/srv/repos"
```

See [Working directory allowlist](#working-directory-allowlist) for what qualifies as a direct child of a root. Live settings apply automatically when you change them with `awman config set`; changing a named listener's `listen` address needs a restart.

---

## Security

API mode preserves all of awman's container isolation guarantees: every agent runs inside a container on the configured runtime, never directly on the host.

The network listener enforces named API key authentication on every request. The plaintext key is returned once by `awman daemon api keys.create`; only its hash is stored. See [Authentication](#authentication) for the key lifecycle.

The working directory allowlist is the secondary access control on the server: even a client that presents a valid API key can only create sessions in pre-approved directories.

The network listener binds to `daemon.network.listeners.<name>.listen` and uses TLS. See [The daemon](16-daemon.md#network-listener) for configuration and listener security.

---

## Session cleanup

At server startup, awman automatically purges sessions that were closed more than 24 hours ago. Their rows are removed from `$AWMAN_HOME/data/awman.db` along with all associated command records. The on-disk output logs and workflow snapshots in `~/.awman/daemon/sessions/<session-id>/` are **not** deleted — they remain for audit purposes, but the API no longer serves a snapshot after its command row is purged. Remove these directories separately when disk retention is no longer needed.

The cleanup runs once, before the server accepts connections, and logs each purged session at `INFO` level. Sessions closed within the last 24 hours are not touched.

---

## Graceful shutdown

On `SIGTERM` or `SIGINT`, the daemon stops accepting new work and drains running commands without a time limit. `awman daemon stop` waits for that drain; `--now` cancels running work. A second shutdown signal escalates a drain to cancellation. Shutdown start and completion are logged.

---

## API edge cases

Cases not covered in the sections above:

| Situation | Behaviour |
|-----------|-----------|
| A `workDirs.allow` path does not exist at startup | Startup refused; create the directory first |
| A `workDirs.roots` path does not exist at startup | Warning logged; the root is checked again on each request |
| A `workDirs.roots` entry is itself inside a Git repository | Startup refused (exit 2); use `workDirs.allow` for a single repository |
| `exec workflow` with `{{work_item_content}}` and no `--work-item` | Warning; placeholder left unexpanded; the workflow continues |
| Bare `"subcommand": "exec"` (action in `args`) | HTTP 400 `invalid_request`; use `exec prompt` or `exec workflow` as the `subcommand` |
| Session closed between listing it and submitting a command | HTTP 404 `session_not_found` |
| `GET /v1/worker/sessions?status=active` with no active sessions | HTTP 200 `{"items":[],"next_cursor":null}` |
| `GET /v1/worker/commands/:id/workflow` for a paused workflow | HTTP 200; keep polling, since the workflow can resume |

---

# Remote mode

`awman remote` is a command-line client for an awman API server. It creates and closes sessions, submits `exec prompt` and `exec workflow` commands, and streams their output to your terminal. It runs from the CLI or the TUI command box, and takes the same flags in both.

---

## Overview

One machine runs `awman daemon start` with the worker feature enabled (the _remote host_); developers or pipelines run `awman remote` (the _client_). The remote host manages all sessions and containers.

```
Client                                           Remote host
──────                                           ───────────
awman remote exec workflow my.toml -f ──────────► POST /v1/worker/commands
                                                 ◄── SSE stream: log output
                                                 ◄── `event: done` when the command ends
```

| Command | What it does |
|---------|-------------|
| `awman remote session start` | Create a session on the remote host and wait until it is ready |
| `awman remote exec workflow <file>` | Submit a workflow to a session |
| `awman remote exec prompt <prompt>` | Send a one-shot prompt to a session |
| `awman remote session kill <session-id>` | Close a session |

The network listener uses TLS and binds to `daemon.network.listeners.<name>.listen`. See [The daemon](16-daemon.md#network-listener).

---

## Connecting to a remote host

A **remote target** is a named server address plus its API key, stored under `remote.targets` in your global config. Every `remote` subcommand (and `awman daemon api`) uses one target:

| Priority | Source |
|----------|--------|
| 1 | The global `--remote <target>` flag |
| 2 | `remote.default` in `~/.awman/config.json` |

With neither set, the command fails and names `--remote` and `remote.default`. A name that is not in `remote.targets` fails naming `remote.targets.<name>`. Define a target and make it the default once:

```sh
awman config set --global remote.targets.build.url https://127.0.0.1:9876
awman config set --global remote.default build
```

---

## API key authentication

The key belongs to the target, so it is only ever sent to that target's URL:

```sh
awman config set --global remote.targets.build.apiKey <your-api-key>
```

The key is a sensitive setting: `awman config show` masks it. For CI, supply the target without a file through the derived environment variables (the target name is upper-cased, `-` becomes `_`):

```sh
export AWMAN_REMOTE_TARGETS_BUILD_URL=https://127.0.0.1:9876
export AWMAN_REMOTE_TARGETS_BUILD_API_KEY=<your-api-key>
export AWMAN_REMOTE_DEFAULT=build
```

Environment variables are visible to other processes of the same user (for example `/proc/<pid>/environ` on Linux). In sensitive contexts prefer the config file with restricted permissions (`chmod 600 ~/.awman/config.json`).

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
| `--remote <target>` | Global flag: the [remote target](#connecting-to-a-remote-host) to use |

On success awman prints `Session created: <id>`, the setup stages, the same ready summary box `awman ready` draws, and finally on stdout:

```
Session ses_a1b2c3d4e5f67890abcdef1234567890 created via https://127.0.0.1:9876 [ready].
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
| `--remote <target>` | | Global flag: the remote target (default `remote.default`) |
Every other flag is an [`exec workflow`](#awman-exec-workflow-path--awman-exec-wf-path) flag, and every one you pass is sent to the server, which runs `exec workflow` with it: `--work-item`, `--issue`, `--agent`, `--model`, `--yolo`, `--auto`, `--plan`, `--overlay`, `--agent-flag`, `--dynamic` and `--leader`, the `--on-…` choices, the `--prerequisite-…` grants and `--check-prerequisites`. Paths in them (`--overlay`, the workflow file) are paths on the remote host,, and `--agent-flag` replaces the remote host's `agents.flags` entry for the run. To change a setting for one run, add `--set key=value` (for example `--set workflows.maxConcurrentSteps=4`); it is forwarded to the server. The server then applies its API defaults (headless, and `--yolo` unless you passed `--plan` or `--auto`); see [Submit a command](#submit-a-command).

Two `exec workflow` flags are not available, because they describe the local run: `--worktree` (the server decides; its API default `--yolo` implies one) and `--json` (the server reports the result; read it with `GET /v1/worker/commands/<id>`). Passing either is an unknown-flag error.

With `--dynamic`, omit the workflow path; the server's leader agent designs the workflow for `--work-item` or `--issue`:

```sh
awman remote exec workflow --dynamic --work-item 0042 --session abc123 -f
```

Without `--session` or `AWMAN_REMOTE_SESSION`, the command fails with ``no remote session id; pass --session <id> or run `awman remote session start` first``.

### Live log streaming (`--follow`)

Without `--follow`, awman prints `Command submitted: <id>` and returns. With `--follow`, it streams the command's events: stdout and stderr lines go to the matching stream (prefixed with `[step]` once more than one workflow step has produced output), and status, step and phase transitions are printed as `[...]` lines. When the command ends it prints a final line on stdout:

```
Command cmd_e5f6a7b8…: workflow (session abc123) via https://127.0.0.1:9876 [done].
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
awman remote session kill ses_a1b2c3d4e5f67890abcdef1234567890
```

On success awman prints `Session <id> killed.` A session the server does not know is treated as already closed.

---

## Remote client configuration

Remote settings live under `remote` in the global config (`~/.awman/config.json`):

```json
{
  "remote": {
    "default": "build",
    "targets": {
      "build": {
        "url": "https://127.0.0.1:9876",
        "apiKey": "a3f8b2c1...64-char-hex..."
      }
    }
  }
}
```

| Key | Meaning |
|-----|---------|
| `remote.default` | Name of the target used when `--remote` is not given |
| `remote.targets.<name>.url` | The server address for that target |
| `remote.targets.<name>.apiKey` | The key sent only to that target (sensitive) |

---

## Full example: CI pipeline

```sh
export AWMAN_REMOTE_TARGETS_BUILD_URL=https://127.0.0.1:9876   # tunnelled to the build host
export AWMAN_REMOTE_TARGETS_BUILD_API_KEY=<your-api-key>
export AWMAN_REMOTE_DEFAULT=build

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
| `--remote` names a target that is not in `remote.targets` | The command fails naming `remote.targets.<name>` |

---

# Upgrading from earlier betas

A one-time reference for clients written against earlier betas. Everything in the "Old" columns is gone; the old routes answer `404 not_found`. Nothing is aliased.

### Routes

| Old | New |
|-----|-----|
| `GET /v1/workdirs` | `GET /v1/worker/workdirs` (now `{items:[{path, kind}], next_cursor}`) |
| `GET /v1/sessions`, `POST /v1/sessions` | `GET /v1/worker/sessions`, `POST /v1/worker/sessions` |
| `GET /v1/sessions/{id}`, `DELETE /v1/sessions/{id}` | `GET /v1/worker/sessions/{id}`, `DELETE /v1/worker/sessions/{id}` |
| `GET /v1/sessions/{id}/status` | removed — use `GET /v1/worker/sessions/{id}`; session requests use `session_type` |
| `GET /v1/sessions/{id}/queue` | `GET /v1/worker/sessions/{id}/queue` |
| `GET /v1/commands`, `POST /v1/commands` | `GET /v1/worker/commands`, `POST /v1/worker/commands` |
| `GET /v1/commands/{id}` | `GET /v1/worker/commands/{id}` |
| `GET /v1/commands/{id}/status` | use `GET /v1/worker/commands/{id}` for command status |
| `POST /v1/commands/{id}/cancel` | `POST /v1/worker/commands/{id}/cancel` |
| `GET /v1/commands/{id}/logs` | `GET /v1/worker/commands/{id}/events` (same SSE stream; `?after=`, `Last-Event-ID`, `?step=` and `?format=json` unchanged) |
| `GET /v1/workflows/{command_id}`, `GET /v1/commands/{id}/workflow` | `GET /v1/worker/commands/{id}/workflow` |
| `GET /v1/commands/{id}/outputs[/{path}]` | `GET /v1/worker/commands/{id}/outputs[/{path}]` |
| `POST /v1/runs` | `POST /v1/worker/runs` |
| Squad daemon `POST /v1/commands` with `{subcommand, args}` | `/v1/squad/tasks`, `/v1/squad/tasks/{id}/runs`, `/v1/squad/runs/{id}/cancel` (see the [route tree](#route-tree)) |
| Squad daemon `GET /v1/tasks/{name}/workflow` | `GET /v1/squad/tasks/{id}/workflow` (find the ID with `GET /v1/squad/tasks?name=<name>`) |
| Squad daemon `GET`/`POST`/`DELETE /v1/daemon/env` | `GET`/`PUT`/`DELETE /v1/squad/env` (`PUT` merges values in) |
| `awman config prerequisite-grants list\|revoke` | `GET /v1/grants`, `DELETE /v1/grants/{id}`, or `awman daemon api grants.list` / `grants.delete id=…` |

### Fields and behaviour

| Old | New |
|-----|-----|
| `non_interactive` in `flags_applied` | `headless` |
| `--non-interactive` in a command's `args` | `unknown_flag`. Use the typed body field `"interaction": "headless" \| "interactive"` |
| `GET /v1/status` `features` (list of strings) | `capabilities`; `features` is now an object of feature states |
| `GET /v1/status` `uptime_seconds`, `active_sessions`, `running_commands` | `uptime_ms`; per-feature `running` and `queued` under `features` |
| `{"sessions": [...]}`, `{"commands": [...]}` | `{"items": [...], "next_cursor": …}`; default page 50, maximum 500 (the old list default was 100, maximum 1000) |
| `status=pending` filter on commands | `status=queued` |
| Unprefixed UUID IDs (`a1b2c3d4-…`) | `ses_…`, `cmd_…`, `task_…`, `run_…`, `grant_…` |
| Squad `interval_secs`, string timestamps | `interval_ms`; RFC 3339 UTC `…_at` timestamps |
| Label keys such as `ci-run` | `<namespace>/<name>` such as `ci/run` for new writes; stored labels remain readable |
| Setup failure of a `POST /v1/worker/runs` command: `error` held a message | `error` is the literal `"setup_error"`; details are in `setup_error` and the structured `failure` |
| `step_activity` payload `{"type", "kind", "summary"}` | `{"type": "StepActivity", "data": {"kind", "summary"}}` |
| Global worker-count setting | `daemon.maxConcurrentJobs` |
| former worker settings | `daemon.features.worker.push.workDirs.allow` / `.roots` / `.allowOverride` (with `.interaction`) |
| `AWMAN_API_VERBOSE_SETUP`, `AWMAN_API_ROOT` | `daemon.verboseSetup`; `AWMAN_HOME` |
| `awman remote … --remote-addr`, `--api-key`, `AWMAN_REMOTE_ADDR`, `AWMAN_API_KEY`, `remote.defaultAddr`, `remote.defaultAPIKey` | `--remote <target>`, `remote.default`, `remote.targets.<name>.url` / `.apiKey` |
| `awman exec … --max-concurrent` | `--set workflows.maxConcurrentSteps=N` |

### Reconciling IDs

On the first start after upgrading, awman rewrites the stored database and the on-disk session and command folders: each old UUID becomes the prefixed ID with the same hex digits and no dashes (`a1b2c3d4-e5f6-7890-abcd-ef1234567890` is now `ses_a1b2c3d4e5f67890abcdef1234567890` for a session or `cmd_…` for a command), and existing grants gain `grant_` IDs. Records, labels, timestamps and logs are kept.

Your **client** still holds the old strings, and the old form is not accepted: requesting `/v1/worker/commands/<old-uuid>` is `404 not_found`. To reconcile, either convert the stored ID yourself (prefix plus the UUID with dashes removed), or look the record up again by label — `GET /v1/worker/commands?label=<your key>=<value>` or `GET /v1/worker/sessions?label=<key>=<value>` — which is why tagging work with [labels](#labels) at creation is worth doing. Listing with `GET /v1/worker/sessions` or `GET /v1/worker/commands` also works if you have no labels.

---

[← Overlays](08-overlays.md) · [Next: GitHub Integration →](10-github-integration.md)


### Worker lease and event conventions

`GET /v1/worker/servers` accepts `limit` and `cursor`, and every returned `next_cursor` can be followed. To test for leases that still need credentials, request `/v1/worker/leases?server=NAME&state=active&limit=1`; the default lease list also includes ended history.

The production daemon registers linked work servers and runs automatic and manual claims on its worker share. Pull routes remain local-only. Unsupported lease execution requirements are released as ineligible; see [Workers and Work Servers](17-workers-and-work-servers.md#running-pulled-work). Expiry or revocation fences execution, events, credentials, push and result reporting together.

SSE, `?format=json` and WSP forward the same public event projection: `emitted_at` is an RFC 3339 UTC timestamp, and `payload.type` is snake_case. Stored older logs with `timestamp` and Rust-style tags such as `StdoutLine` are read compatibly and projected on replay. Clients should accept the public spelling for new streams.
