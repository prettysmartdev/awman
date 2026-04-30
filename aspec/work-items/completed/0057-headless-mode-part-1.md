# Work Item: Feature

Title: headless mode part 1
Issue: issuelink

## Summary:
- Introduce a third execution mode for amux: **headless mode**. When `amux headless start` is invoked, amux launches an HTTP server that exposes session management and subcommand execution to remote clients. A session is conceptually identical to a TUI tab — a named, isolated workspace bound to a working directory. Subcommands dispatched to a session execute exactly as they would inside a TUI tab. All operations, inputs, and outputs are stored durably in `~/.amux/headless/` for auditability. The server can be run in the foreground or daemonized into the OS process manager via `--background`.


## User Stories

### User Story 1:
As a: developer building automation or integrations on top of amux

I want to: start a persistent amux HTTP server with `amux headless start` and drive sessions and subcommands via HTTP requests

So I can: integrate amux capabilities (implement, chat, ready, etc.) into CI pipelines, scripts, or remote tooling without needing an interactive terminal or TUI session.

### User Story 2:
As a: developer running headless amux in the background

I want to: use `amux headless start --background` to daemonize the server via the OS process manager, and later use `amux headless kill` and `amux headless logs` to control and inspect it

So I can: run the amux headless server as a persistent background service that survives terminal disconnects, with all output captured to `~/.amux/headless/amux.log`.

### User Story 3:
As a: security-conscious operator or auditor

I want to: find a complete durable record of every HTTP request received, every session created, every subcommand run, and every result produced — including timestamps, unique IDs, working directories, container names, and full stdout/stderr — in `~/.amux/headless/`

So I can: audit what the amux headless server did, diagnose failures after the fact, and be confident that nothing happened silently or without a trace.


## Implementation Details:

### New CLI surface (`src/cli.rs`)
- Add `Command::Headless { action: HeadlessAction }` to the `Command` enum.
- `HeadlessAction` sub-enum:
  - `Start { port: u16, workdirs: Vec<String>, background: bool }` — launch the HTTP server. `--port` defaults to `9876`. `--workdirs` accepts one or more absolute paths (repeatable flag). `--background` daemonizes via the OS process manager.
  - `Kill` — signal the background server process to stop via the OS process manager.
  - `Logs` — stream the background log file (`~/.amux/headless/amux.log`) to stdout, following new content like `tail -f`.
  - `Status` — print whether the server is running, its PID, port, active sessions, and uptime.
- Wire the new command into `src/commands/mod.rs`.

### New command module (`src/commands/headless/`)
- `mod.rs` — top-level dispatch: `run_start`, `run_kill`, `run_logs`, `run_status`.
- `server.rs` — the axum HTTP server (router, handlers, shared state).
- `db.rs` — SQLite schema setup and all data access functions (sessions table, commands table).
- `process.rs` — OS process manager integration for `--background`, `kill`, and PID management.
- `logging.rs` — structured tracing setup; routes logs to stdout in foreground mode and to `~/.amux/headless/amux.log` in background mode.

### Storage layout (`~/.amux/headless/`)
```
~/.amux/headless/
  amux.log                  # server log (background mode)
  amux.pid                  # PID file for the background process
  amux.db                   # SQLite database (sessions + commands)
  sessions/
    <session-uuid>/
      worktree/             # git worktree for this session (if applicable)
      agent-settings/       # sanitized host settings for container runs
      commands/
        <command-uuid>/
          stdout.log        # captured stdout
          stderr.log        # captured stderr
          metadata.json     # command request, flags, start/end times, exit code
```

### SQLite schema (`db.rs`)
**`sessions` table:**
- `id` TEXT PRIMARY KEY (UUID v4)
- `workdir` TEXT NOT NULL
- `created_at` TEXT NOT NULL (ISO 8601)
- `status` TEXT NOT NULL (`active` | `closed`)
- `closed_at` TEXT

**`commands` table:**
- `id` TEXT PRIMARY KEY (UUID v4)
- `session_id` TEXT NOT NULL REFERENCES sessions(id)
- `subcommand` TEXT NOT NULL (e.g. `implement`, `chat`)
- `args` TEXT NOT NULL (JSON array)
- `status` TEXT NOT NULL (`pending` | `running` | `done` | `error`)
- `exit_code` INTEGER
- `started_at` TEXT
- `finished_at` TEXT
- `stdout_path` TEXT NOT NULL
- `stderr_path` TEXT NOT NULL

### HTTP API (`server.rs`)
All responses are JSON. All endpoints log at `INFO` or above.

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/workdirs` | List the server's allowlisted working directories |
| `POST` | `/v1/sessions` | Create a new session. Body: `{ "workdir": "<path>" }`. Returns `{ "session_id": "<uuid>" }` |
| `GET` | `/v1/sessions` | List all sessions (active and closed) with metadata |
| `GET` | `/v1/sessions/:id` | Get session detail |
| `DELETE` | `/v1/sessions/:id` | Close a session (marks as closed, does not destroy data) |
| `POST` | `/v1/commands` | Submit a subcommand. Requires `x-amux-session` header with session UUID. Body: `{ "subcommand": "implement", "args": ["0057"] }`. Returns `{ "command_id": "<uuid>" }` immediately (async execution) |
| `GET` | `/v1/commands/:id` | Get command status and metadata |
| `GET` | `/v1/commands/:id/logs` | Stream or return captured command logs |
| `GET` | `/v1/status` | Server health: uptime, active session count, running command count |

### Working directory allowlist
- At startup, `--workdirs` values (and `headlessWorkDirs` from `GlobalConfig`) are resolved to canonical absolute paths and stored in server state.
- `POST /v1/sessions` must reject any `workdir` not in the allowlist with HTTP 403 and a descriptive error.
- Add `headless_work_dirs: Option<Vec<String>>` (serialized as `headlessWorkDirs`) to `GlobalConfig` in `src/config/mod.rs`.

### Subcommand execution
- When a `POST /v1/commands` request arrives, the server:
  1. Validates the session exists and is active; validates the subcommand name.
  2. Creates a new `commands` row with status `pending` and generates a UUID.
  3. Creates the per-command directory under `~/.amux/headless/sessions/<session-uuid>/commands/<command-uuid>/`.
  4. Spawns a Tokio task to run the subcommand via the existing `commands::run()` dispatch path, capturing stdout and stderr to the per-command log files.
  5. Updates `status`, `exit_code`, and timestamps in the DB when the task completes.
  6. Returns `{ "command_id": "<uuid>" }` immediately to the client (fire-and-forget from the client's perspective).
- Execution is isolated per session: each session has its own `workdir` context. Every command executed must use the session's workdir as its execution dir.

### Background mode and process management (`process.rs`)
- **Linux (systemd available):** write a transient systemd unit via `systemd-run --user` and start it. Store PID in `~/.amux/headless/amux.pid`.
- **macOS (launchd):** write a `launchd` plist to `~/Library/LaunchAgents/io.amux.headless.plist`, run `launchctl load`, store PID.
- **Fallback (no systemd/launchd):** double-fork and write PID file directly.
- `amux headless kill`: read `amux.pid`, send `SIGTERM`, remove PID file. On macOS also unload the launchd plist.
- `amux headless logs`: open `~/.amux/headless/amux.log` and stream new bytes to stdout in a loop (equivalent to `tail -f`).

### Logging
- In foreground mode: configure `tracing-subscriber` to emit structured JSON or human-readable logs to stdout/stderr.
- In background mode: configure `tracing-subscriber` to write to `~/.amux/headless/amux.log` (append, with rotation guard on size if feasible).
- Log every HTTP request (method, path, headers relevant to routing, response status, latency).
- Log every session create/close event with session UUID and workdir.
- Log every command dispatch and completion with command UUID, session UUID, subcommand, args, exit code, and paths to stdout/stderr files.
- Log server startup with port, allowlisted workdirs, PID, and storage root.
- Add periodic heartbeat log line every 60 seconds (active sessions, running commands).

### New dependencies (`Cargo.toml`)
- `axum = "0.7"` — async HTTP framework (tokio-native, no additional async runtime needed)
- `rusqlite = { version = "0.31", features = ["bundled"] }` — SQLite, bundled to keep the binary self-contained
- `uuid = { version = "1", features = ["v4"] }` — UUID generation for session and command IDs
- `tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }` — structured logging (move from dev-dependencies to main dependencies)
- `tower-http = { version = "0.5", features = ["trace"] }` — HTTP request tracing middleware for axum


## Edge Case Considerations:
- **Workdir not in allowlist:** `POST /v1/sessions` must return HTTP 403 with a JSON error body listing the allowlisted directories. Never silently accept or canonicalize to a different path.
- **Session not found or closed:** `POST /v1/commands` with an unknown or closed session UUID must return HTTP 404. Include the session UUID in the error body for debuggability.
- **Concurrent subcommands on same session:** two `POST /v1/commands` requests for the same session results in the second request returning a 403 error response. Use an in-memory mutex on the session/subcommand tracking variable to ensure that two concurrent request cannot result in two concurrently executing subcommands. When a running subcommand exits, the session may recieve new subcommand requests.  
 **Server already running on startup:** before binding, detect if `amux.pid` exists and the process is alive. Print an error and exit with a non-zero code rather than silently competing for the port.
- **Port already in use:** if `bind()` fails with EADDRINUSE, emit a clear error message including the port number and the PID that holds it (if discoverable).
- **Graceful shutdown:** on `SIGTERM` / `SIGINT`, finish in-flight HTTP responses and allow running commands up to a configurable grace period (default 30 s) before force-killing Tokio tasks. Log shutdown start and completion.
- **Allowlist path canonicalization:** resolve `--workdirs` values through `std::fs::canonicalize` at startup to handle trailing slashes and symlinks consistently. Log a warning (but do not abort) if a path does not exist at startup.
- **SQLite write contention:** use `rusqlite::Connection` behind a `tokio::sync::Mutex` (single writer) to avoid SQLITE_BUSY errors under concurrent command submissions.
- **Large stdout/stderr:** write output incrementally as the subprocess produces it; do not buffer entire output in memory. Use async file I/O (tokio::fs) for the log files.
- **`amux headless kill` when server is not running:** check for PID file absence and non-running PID; print a clear message rather than silently succeeding or panicking.
- **`amux headless logs` when no log file exists:** print a clear error message telling the user to start the server with `--background` first.
- **Unknown subcommand in POST /v1/commands:** validate the subcommand name against the known set (`implement`, `chat`, `ready`, etc.) before dispatching. Return HTTP 400 with a list of valid subcommands.


## Test Considerations:
- **Unit tests — `db.rs`:** test schema creation, session insert/query/close, command insert/status-update, UUID uniqueness, and round-trip of all fields through serde.
- **Unit tests — `process.rs`:** test PID file write/read/delete, detection of a running vs. stopped process by PID, and error handling when the PID file is absent.
- **Unit tests — config:** test that `headlessWorkDirs` deserializes from `GlobalConfig` JSON and round-trips through `save_global_config` / `load_global_config`. Test that a missing field produces an empty Vec (not an error).
- **Unit tests — CLI parsing:** for each `HeadlessAction` variant, test that flags parse correctly: `--port`, `--workdirs` (single and multiple values), `--background`. Test defaults (port 9876, no workdirs, background false).
- **Integration tests — HTTP API:** spin up the server on a random port in a test, run the full session + command lifecycle: create session → submit command → poll status → retrieve stdout. Assert DB state matches HTTP responses.
- **Integration tests — allowlist enforcement:** start server with one allowlisted dir, attempt to create a session with a different dir, assert HTTP 403.
- **Integration tests — `--background` flag:** on Linux, verify that `--background` writes a PID file, the process is running, and `amux headless kill` terminates it and removes the PID file. Mark these tests `#[ignore]` if systemd/launchd is unavailable in the test environment.
- **End-to-end test:** invoke `amux headless start` in a subprocess on a temp port, send HTTP requests via `reqwest` (already in dependencies), verify the response shape and that log files and DB entries were created on disk.
- **Test infrastructure:** use `tempfile::TempDir` for all `~/.amux/headless/` paths in tests by overriding the storage root via an environment variable (`AMUX_HEADLESS_ROOT`) recognized in `db.rs` and `process.rs`.


## Codebase Integration:
- Follow the established `Command::Variant { action: ActionEnum }` pattern used by `Command::Claws` and `Command::Specs` in `src/cli.rs`. Mirror the dispatch pattern in `src/commands/mod.rs`.
- The `GlobalConfig` struct in `src/config/mod.rs` needs a new `headless_work_dirs` field (`#[serde(rename = "headlessWorkDirs", skip_serializing_if = "Option::is_none")]`). Follow the same pattern as `yolo_disallowed_tools` and `env_passthrough`.
- The new `src/commands/headless/` module must be registered with `pub mod headless;` in `src/commands/mod.rs`.
- Use `tracing::info!`, `tracing::warn!`, `tracing::error!` consistently — the project already pulls in `tracing = "0.1"` as a dependency.
- `tracing-subscriber` is currently a dev-dependency; promote it to a regular dependency when adding the headless logging setup.
- Axum requires `tokio` with the `full` feature set, which is already enabled in `Cargo.toml`.
- `rusqlite` with `features = ["bundled"]` compiles SQLite from source, keeping the binary self-contained and matching the existing constraint of a single statically-linked binary.
- All file I/O within the headless server should use `tokio::fs` (async) to avoid blocking the Tokio executor; synchronous `std::fs` calls should be restricted to startup/shutdown paths.
- Use `uuid::Uuid::new_v4().to_string()` for all session and command IDs; store as TEXT in SQLite.
- For the spec parity test infrastructure in `src/cli.rs` (work item 0053), add `HEADLESS_START_FLAGS` (and any other subcommand flag lists) to `src/commands/spec.rs` and wire up the corresponding CLI/spec parity tests following the existing pattern for `CHAT_FLAGS`, `IMPLEMENT_FLAGS`, etc.
