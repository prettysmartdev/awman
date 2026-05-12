# Work Item: Task

Title: grand architecture refactor — Headless frontend + headless/remote/auth command bodies + TLS engine
Issue: n/a — seventh-of-eight work item implementing `aspec/architecture/2026-grand-architecture.md`

## Prerequisites

All Layer 0 (data), Layer 1 (engine), Layer 2 (command/dispatch), and the CLI frontend (Layer 3) are complete and tested in prior work items (0066–0070). The TUI frontend is complete (WI 0071). The CLI frontend in `src/frontend/cli/` serves as the reference implementation for how a frontend implements the trait system.

This work item does NOT depend on any types or code from the TUI frontend. The headless frontend implements the same Layer 2 traits (defined in `src/command/commands/`) as the CLI and TUI — all shared types are in Layer 0/1/2.

The implementing agent MUST read:

- `aspec/architecture/2026-grand-architecture.md` end-to-end — source of truth for the layered architecture.
- The current state of `src/data/`, `src/engine/`, `src/command/`, and `src/frontend/cli/` — the real layers the headless frontend calls into.
- `oldsrc/commands/headless/server.rs` end-to-end — the legacy headless server whose HTTP API must be wire-identical in the new implementation.
- `oldsrc/commands/headless/` (mod.rs, auth.rs, db.rs, logging.rs, process.rs) — the legacy headless infrastructure being ported.
- `oldsrc/commands/remote.rs` and `oldsrc/commands/auth.rs` — the legacy command bodies being ported.

## Architecture tenets

These four tenets govern every decision in this work item:

1. **Frontends contain NO business logic.** The headless frontend translates HTTP requests into `CommandFrontend` method calls and renders typed outcomes as HTTP responses. That is all.
2. **Lower layers never call upward.** Layer 1/2 code uses frontend traits to delegate user interaction to Layer 3. The headless frontend implements these traits with safe non-interactive defaults.
3. **Typed objects over `pub fn`.** Build structs with well-understood options that expose public methods.
4. **When uncertain, ASK THE DEVELOPER.** Do not make assumptions about behavior, defaults, or architecture decisions.

## Scope

Three deliverables:

1. **`src/frontend/headless/`** — full headless HTTP server. Wire-identical to `oldsrc/commands/headless/server.rs`; the only internal change is that `POST /v1/commands` dispatches through `Dispatch` instead of spawning a child `amux` process.
2. **Real Layer 2 command bodies** for `headless start/kill/logs/status`, `remote run/session start/session kill`, and the headless-side persistence half of `auth`. These are currently stubbed because they only become meaningful once the headless server exists.
3. **Real `AuthEngine::ensure_self_signed_tls`** — currently returns `EngineError::NotImplemented`. Real `rcgen`-based self-signed cert generation.

After this work item:
- `amux headless start` boots a real HTTP server that serves the legacy API
- `amux headless kill/logs/status` manage it
- `amux remote *` talk to it from another host
- `amux auth` round-trips through the global config persistence layer cleanly
- `RemoteClient::stream_command()` is real (currently returns `NotImplemented`)

---

## 1. Layer 0 additions — headless persistence and paths

The headless server needs persistence infrastructure that does not yet exist at Layer 0. Add these before building the headless frontend.

### 1a. `src/data/fs/headless_paths.rs` — path resolution

Add helpers for all headless-specific file paths:

```rust
pub fn headless_dir(home: &Path) -> PathBuf          // <HOME>/.amux/headless/
pub fn pid_file(home: &Path) -> PathBuf               // <HOME>/.amux/headless/amux.pid
pub fn log_file(home: &Path) -> PathBuf               // <HOME>/.amux/headless/amux.log
pub fn api_key_hash_file(home: &Path) -> PathBuf      // <HOME>/.amux/headless/api-key.hash
pub fn tls_cert_file(home: &Path) -> PathBuf           // <HOME>/.amux/headless/tls/cert.pem
pub fn tls_key_file(home: &Path) -> PathBuf            // <HOME>/.amux/headless/tls/key.pem
pub fn command_dir(home: &Path, cmd_id: &str) -> PathBuf  // <HOME>/.amux/headless/commands/<cmd_id>/
pub fn command_output_log(home: &Path, cmd_id: &str) -> PathBuf  // .../output.log
pub fn command_workflow_state(home: &Path, cmd_id: &str) -> PathBuf  // .../workflow.state.json
```

### 1b. `src/data/headless_db.rs` — SQLite session/command persistence

The headless server tracks sessions and commands in SQLite. Port from `oldsrc/commands/headless/db.rs`:

```rust
pub struct HeadlessDb { /* SQLite connection */ }

impl HeadlessDb {
    pub fn open(path: &Path) -> Result<Self, DataError>;
    pub fn migrate(&self) -> Result<(), DataError>;  // creates tables if missing

    // Sessions
    pub fn create_session(&self, working_dir: &Path) -> Result<HeadlessSessionRow, DataError>;
    pub fn list_sessions(&self) -> Result<Vec<HeadlessSessionRow>, DataError>;
    pub fn get_session(&self, id: &str) -> Result<Option<HeadlessSessionRow>, DataError>;
    pub fn delete_session(&self, id: &str) -> Result<bool, DataError>;

    // Commands
    pub fn create_command(&self, session_id: &str, subcommand: &str, args: &[String]) -> Result<HeadlessCommandRow, DataError>;
    pub fn get_command(&self, id: &str) -> Result<Option<HeadlessCommandRow>, DataError>;
    pub fn update_command_status(&self, id: &str, status: &str) -> Result<(), DataError>;
    pub fn update_command_finished(&self, id: &str, status: &str) -> Result<(), DataError>;
}
```

Schema MUST be forward-compatible with the legacy schema in `oldsrc/commands/headless/db.rs`. Existing databases from pre-refactor installs must load without error.

### 1c. `src/data/headless_process.rs` — PID file lifecycle

Port from `oldsrc/commands/headless/process.rs`:

```rust
pub fn write_pid(pid_path: &Path, pid: u32) -> Result<(), DataError>;
pub fn read_pid(pid_path: &Path) -> Result<Option<u32>, DataError>;
pub fn clear_pid(pid_path: &Path) -> Result<(), DataError>;
pub fn pid_is_amux(pid: u32) -> bool;  // checks if process name contains "amux"
```

The "spawn background" helper is OS-specific:
- `cfg(unix)`: `fork` + `setsid` + nohup pattern (port verbatim from oldsrc)
- `cfg(windows)`: `CREATE_NEW_PROCESS_GROUP` + `CreateProcessW` (port verbatim from oldsrc)

```rust
pub fn spawn_background(binary_path: &Path, args: &[String]) -> Result<u32, DataError>;
```

---

## 2. `src/frontend/headless/` — files and structure

Build these files under `src/frontend/headless/`:

### `mod.rs` — Entry point

```rust
pub async fn serve(
    config: HeadlessServeConfig,
    engines: Engines,
    session_manager: Arc<RwLock<SessionManager>>,
    db: Arc<HeadlessDb>,
) -> Result<(), HeadlessError>
```

- Builds the Axum router via `routes::build_router`
- Binds to `config.port` on `config.bind_addr`
- When TLS is enabled: loads cert/key from `AuthEngine::ensure_self_signed_tls` material
- When auth is enabled: installs the auth middleware
- Blocks until SIGINT/SIGTERM
- Graceful shutdown: 30-second grace period for running commands

**Layer 2 cannot call `serve` directly** — that would be an upward call. The headless `start` command (Layer 2) accepts a `HeadlessStartCommandFrontend` trait; the CLI frontend's `serve_until_shutdown()` impl calls `crate::frontend::headless::serve(...)`. This is a peer call within Layer 3 (allowed).

### `routes.rs` — HTTP route registration

Registers the **same routes as `oldsrc/commands/headless/server.rs::build_router`**, verbatim. Route list is fixed and NOT derived from `CommandCatalogue`:

| Method | Path | Handler |
|--------|------|---------|
| GET | `/v1/status` | Server status (version, uptime) |
| GET | `/v1/workdirs` | List allowed working directories |
| GET | `/v1/sessions` | List all sessions |
| POST | `/v1/sessions` | Create a new session |
| GET | `/v1/sessions/:id` | Get session details |
| DELETE | `/v1/sessions/:id` | Delete a session |
| POST | `/v1/commands` | Create and execute a command |
| GET | `/v1/commands/:id` | Get command status |
| GET | `/v1/commands/:id/logs` | Get command output (full) |
| GET | `/v1/commands/:id/logs/stream` | Stream command output (SSE) |
| GET | `/v1/workflows/:command_id` | Get workflow state |

The `POST /v1/commands` handler replaces the legacy child-process spawn with a Dispatch call:

```rust
// Legacy: spawns `amux <subcommand> <args...>` as a subprocess
// New: directly calls into Layer 2
let frontend = HeadlessCommandFrontend::new(req.subcommand, req.args, log_path);
let command_path = frontend.parse_command_path()?;
let dispatch = Dispatch::new(frontend, session, engines);
dispatch.run_command(&command_path).await
```

All surrounding logic (session validation, concurrency guard, `x-amux-session` header, DB inserts, command directory creation, 202 Accepted response) is ported verbatim from `oldsrc/commands/headless/server.rs::handle_create_command` and `execute_command`.

### `command_frontend.rs` — HeadlessCommandFrontend

`HeadlessCommandFrontend` implementing `CommandFrontend` + all per-command frontend traits.

- Constructed from `CreateCommandRequest { subcommand: String, args: Vec<String> }`
- `parse_command_path(&self) -> Result<CommandPath, HeadlessError>` — validates subcommand against `CommandCatalogue`
- `CommandFrontend::flag_*` methods: parses remaining `args` against the command's known flags (same as CLI, but from HTTP request body instead of argv)
- For interactive Q&A methods: returns the safe non-interactive defaults (see §5 below)
- Each default MAY be overridden by request body parameters (ASK THE DEVELOPER which ones)

### `container_log.rs` — HeadlessContainerFrontend

`HeadlessContainerFrontend` implementing `ContainerFrontend`:

- `write_stdout(&mut self, bytes: &[u8])` → appends to the command's `output.log` file
- `write_stderr(&mut self, bytes: &[u8])` → appends to the same `output.log` file
- `read_stdin` → returns EOF immediately (no interactive input in headless mode)
- `report_status`, `report_progress` → no-ops
- `resize_pty` → no-op

The `GET /v1/commands/:id/logs/stream` SSE endpoint streams from the `output.log` file:
- Line-per-`data:` event format
- Terminated by `[amux:done]` sentinel
- **Wire format must be byte-identical to old-amux**

### `workflow_state.rs` — HeadlessWorkflowFrontend

`HeadlessWorkflowFrontend` implementing `WorkflowFrontend`:

- `user_choose_next_action` → returns `NextAction::LaunchNext` (non-yolo) or auto-advance (yolo)
- `user_choose_after_step_failure` → returns `StepFailureChoice::Pause`
- `confirm_resume` → returns `true`
- All `report_*` methods → write state to `workflow.state.json` in the command directory
- `yolo_countdown_tick` → returns `YoloTickOutcome::Continue`

The `GET /v1/workflows/:command_id` endpoint reads from `workflow.state.json`; JSON schema must be identical to old-amux.

### `user_message.rs` — HeadlessUserMessageSink

`HeadlessUserMessageSink` implementing `UserMessageSink`:

- Emits each message as an SSE event of type `amux-message`:
  ```json
  { "level": "info"|"warning"|"error"|"success", "text": "..." }
  ```
- `replay_queued()` is a no-op (messages are streamed live)

### `worktree_lifecycle_frontend.rs` — HeadlessWorktreeLifecycleFrontend

`HeadlessWorktreeLifecycleFrontend` implementing `WorktreeLifecycleFrontend`:

- Uses safe non-interactive defaults for all decisions (see §5)
- Reports worktree events as `amux-message` SSE events
- ASK THE DEVELOPER whether to expose Q&A decisions as separate API endpoints or as upfront request parameters

### `auth.rs` — TLS + API-key middleware

Pure HTTP plumbing; all cryptographic logic lives in `AuthEngine` (Layer 1).

- **Token mode**: validates `Authorization: Bearer <key>` header against `AuthEngine::verify_api_key()` with constant-time comparison
- **Disabled mode** (`--dangerously-skip-auth`): adds `X-Amux-Auth: disabled` response header
- **TLS-required mode**: rejects non-loopback bind addresses when TLS is not configured

### `errors.rs` — Error translation

Translates `CommandError`, `EngineError`, `HeadlessError` into HTTP status codes + JSON error bodies:

```json
{ "error": { "code": "...", "message": "..." } }
```

### `defaults.rs` — Safe non-interactive defaults

Named constants for every headless default. See §5 for the complete list.

### Serde shapes — wire compatibility

These types MUST have field names, types, and JSON serialization identical to `oldsrc/commands/headless/server.rs`:

- `CreateCommandRequest { subcommand: String, args: Vec<String> }`
- `CreateCommandResponse { id: String, status: String }`
- `SessionResponse { id: String, working_dir: String, created_at: String }`
- `CommandResponse { id: String, session_id: String, subcommand: String, status: String, created_at: String, finished_at: Option<String> }`
- `StatusResponse { version: String, uptime_seconds: u64 }`
- `ErrorResponse { error: ErrorBody }`

Do NOT rename fields, change types, or add/remove fields.

---

## 3. Real Layer 2 command bodies — headless

File: `src/command/commands/headless.rs`. Currently returns placeholder values for all subcommands.

The headless command surface is four subcommands:

### `HeadlessSubcommand::Start`

Flags: `port`, `workdirs`, `background`, `refresh_key`, `dangerously_skip_auth`

Port from `oldsrc/commands/headless/mod.rs::run_start`:

1. Resolve effective `HeadlessServeConfig` from flags + `GlobalConfig::headless`
2. When `--refresh-key`: call `AuthEngine::refresh_api_key()`, print the plaintext key to stderr in the legacy banner format (verbatim from `oldsrc/commands/headless/server.rs::print_refresh_key_banner`), and return. Do NOT proceed to serve. This is legacy behavior.
3. When NOT `--dangerously-skip-auth` and no API key hash exists: error with `CommandError::HeadlessAuthMissing` and hint to run `amux auth --refresh-key`
4. Call `AuthEngine::ensure_self_signed_tls(bind_ip)` to generate/load TLS material
5. When `--background`: daemonize via `spawn_background()` (Layer 0 helper from §1c). Foreground process writes PID file and exits cleanly.
6. When foreground: call `frontend.serve_until_shutdown(config)` — the per-command frontend trait method. The CLI frontend's implementation of `serve_until_shutdown()` calls `crate::frontend::headless::serve(...)` (a Layer 3 peer call). Block until shutdown signal (SIGINT, SIGTERM).
7. On shutdown: remove PID file via `clear_pid()`.
8. Return `HeadlessStartOutcome { bound_addr, refresh_key_printed, background }`

### `HeadlessSubcommand::Kill`

Port from `oldsrc/commands/headless/mod.rs::run_kill`:

1. Read PID from `<HOME>/.amux/headless/amux.pid`
2. Stale-PID detection: if process is not amux (per `pid_is_amux()`), surface `CommandError::HeadlessNotRunning` and clean up the stale file
3. Send SIGTERM; wait up to 5 seconds; SIGKILL if still alive
4. Remove PID file
5. Return `HeadlessKillOutcome { pid, killed }`

### `HeadlessSubcommand::Logs`

Port from `oldsrc/commands/headless/mod.rs::run_logs`:

1. Read `<HOME>/.amux/headless/amux.log`
2. Stream to the supplied `UserMessageSink` (CLI: stdout)
3. Legacy behavior: does NOT tail; cats the file once and exits. Preserve this.
4. Return `HeadlessLogsOutcome { lines_printed }`

### `HeadlessSubcommand::Status`

Port from `oldsrc/commands/headless/mod.rs::run_status`:

1. Check PID file → process exists
2. When process exists: probe `127.0.0.1:<port>` via `GET /v1/status`
3. Return `HeadlessStatusOutcome { running, pid, bound_addr, version }` (last two `Option`)

---

## 4. Real Layer 2 command bodies — remote

Files: `src/command/commands/remote.rs`, `src/command/commands/remote_client.rs`.

Currently `remote.rs` has routing logic but `remote_client.rs::stream_command()` returns `EngineError::NotImplemented`.

### `RemoteSubcommand::Run`

Flags: `command`, `remote_addr`, `session`, `follow`, `api_key`

Port from `oldsrc/commands/remote.rs::run_remote_run`:

1. Resolve effective remote address: `--remote-addr` > env `AMUX_REMOTE_ADDR` > `GlobalConfig::remote.default_addr`. Surface `CommandError::RemoteAddrMissing` when none.
2. Resolve effective API key: `--api-key` > env `AMUX_API_KEY` > `GlobalConfig::remote.default_api_key` ONLY when the resolved address matches `GlobalConfig::remote.default_addr` after URL canonicalization (e.g. `https://example.com:443` and `https://example.com/` are the same).
3. Resolve effective session: `--session` > prompt via `RemoteCommandFrontend::ask_session_picker` if server reports more than one session. When server has zero sessions, error with `CommandError::RemoteSessionMissing` and hint to run `amux remote session start`.
4. Build `CreateCommandRequest { subcommand: command[0], args: command[1..] }`
5. POST via `RemoteClient::send_command`. 202 Accepted → command_id.
6. When `--follow`: call `RemoteClient::stream_command(command_id)` — opens `GET /v1/commands/:id/logs/stream` (SSE), parses each `data:` line, forwards through the supplied `UserMessageSink`. Block until `[amux:done]` sentinel.
7. When NOT `--follow`: return immediately with `RemoteRunOutcome { command_id, address }`

### `RemoteSubcommand::SessionStart`

Flags: `dir`, `remote_addr`, `api_key`

Port from `oldsrc/commands/remote.rs::run_session_start`:

1. Resolve address + API key (same as Run)
2. When `dir` is `None`: prompt via `RemoteCommandFrontend::ask_saved_dir_picker`
3. POST `/v1/sessions { working_dir }`. 200 OK → session id
4. When server confirms a new directory (`created: true`): prompt `RemoteCommandFrontend::confirm_save_dir`. On `true`, append to `GlobalConfig::remote.saved_dirs` and persist.
5. Return `RemoteSessionStartOutcome { session_id, working_dir, saved }`

### `RemoteSubcommand::SessionKill`

Flags: `session_id`, `remote_addr`, `api_key`

Port from `oldsrc/commands/remote.rs::run_session_kill`:

1. Resolve address + API key
2. When `session_id` is `None`: prompt via `RemoteCommandFrontend::ask_session_kill_picker`
3. DELETE `/v1/sessions/:id`. 200/204 OK or 404 (already gone) → success. Other → `CommandError::RemoteSessionKillFailed`.
4. Return `RemoteSessionKillOutcome { session_id }`

### `RemoteClient` real implementations

In `src/command/commands/remote_client.rs`, replace stubs with real HTTP calls:

- `send_command(req) -> Result<RemoteCommandId, ...>` — POST to `/v1/commands`
- `stream_command(command_id, sink) -> Result<RemoteCommandExit, ...>` — GET `/v1/commands/:id/logs/stream` SSE consumer. Parse each `data:` line, forward to `UserMessageSink`, return when `[amux:done]` received.
- `list_sessions(...) -> Result<Vec<SessionInfo>, ...>`
- `create_session(working_dir) -> Result<SessionInfo, ...>`
- `delete_session(id) -> Result<(), ...>`

HTTP timeouts:
- connect: 10 seconds
- read: 600 seconds for `send_command`
- read: disabled (or 24h) for `stream_command`

TLS verification mode:
- When remote address is `127.0.0.1`/`::1` and cert is the locally-stored self-signed cert: accept with SHA-256 fingerprint pinning
- Otherwise: standard webpki verification
- Port the TLS verifier from `oldsrc/commands/remote.rs::tls_verifier`

---

## 5. Headless dialog defaults — exhaustive list

Every interactive frontend method returns a safe non-interactive default when called from the headless frontend. These defaults are named constants in `src/frontend/headless/defaults.rs`:

| Trait | Method | Default |
|-------|--------|---------|
| `ReadyFrontend` | `ask_create_dockerfile` | `true` |
| `ReadyFrontend` | `ask_run_audit_on_template` | `false` |
| `ReadyFrontend` | `ask_migrate_legacy_layout` | `false` |
| `InitFrontend` | `ask_replace_aspec` | `false` |
| `InitFrontend` | `ask_run_audit` | `false` |
| `InitFrontend` | `ask_work_items_setup` | `None` |
| `ClawsFrontend` | `ask_replace_existing_clone` | `false` |
| `ClawsFrontend` | `ask_run_audit` | `false` |
| `WorkflowFrontend` | `user_choose_next_action` | `NextAction::LaunchNext` |
| `WorkflowFrontend` | `user_choose_after_step_failure` | `StepFailureChoice::Pause` |
| `WorktreeLifecycleFrontend` | `ask_pre_worktree_uncommitted_files` | `PreWorktreeDecision::UseLastCommit` |
| `WorktreeLifecycleFrontend` | `ask_existing_worktree` | `ExistingWorktreeDecision::Resume` |
| `WorktreeLifecycleFrontend` | `ask_post_workflow_action` | `PostWorkflowWorktreeAction::Keep` |
| `WorktreeLifecycleFrontend` | `ask_worktree_commit_before_merge` | `None` |
| `WorktreeLifecycleFrontend` | `confirm_squash_merge` | `false` |
| `WorktreeLifecycleFrontend` | `confirm_worktree_cleanup` | `false` |
| `MountScopeFrontend` | `ask_mount_scope` | `MountScope::MountGitRoot` |
| `AgentSetupFrontend` | `ask_agent_setup` | `AgentSetupDecision::Setup` |
| `AgentAuthFrontend` | `ask_agent_auth_consent` | `AuthConsentChoice::DeclineOnce` |
| `RemoteCommandFrontend` | `ask_session_picker` | First session |
| `RemoteCommandFrontend` | `ask_saved_dir_picker` | First saved dir |
| `RemoteCommandFrontend` | `ask_session_kill_picker` | First session |
| `RemoteCommandFrontend` | `confirm_save_dir` | `false` |
| `SpecsCommandFrontend` | `ask_spec_kind` | Error (requires interactive input) |
| `SpecsCommandFrontend` | `ask_spec_title` | Error (requires interactive input) |
| `NewCommandFrontend` | `ask_workflow_name` | Error (requires interactive input) |
| `NewCommandFrontend` | `ask_skill_name` | Error (requires interactive input) |
| `AuthCommandFrontend` | `ask_consent` | `AuthConsentChoice::DeclineOnce` |

For methods that return Error: these commands require interactive input and cannot be run headlessly without explicit parameters. The headless frontend returns `CommandError::InteractiveInputUnavailable` with a message explaining which parameters to supply in the request body instead.

---

## 6. Real `AuthCommand` headless-side persistence

File: `src/command/commands/auth.rs`. The interactive consent half landed in WI 0070; the headless-side bits land here.

### `AuthSubcommand::RefreshApiKey`

(or `AuthCommand` with `--refresh-key` flag — confirm against `oldsrc/commands/auth.rs`):

1. Call `AuthEngine::refresh_api_key()` (real impl per §7 below)
2. Print the new key to stderr in the legacy banner format (verbatim from `oldsrc/commands/headless/server.rs::print_refresh_key_banner`)
3. Return `AuthOutcome { refreshed: true, fingerprint }`

### `AuthSubcommand::Show`

1. Read current API key fingerprint from hash file
2. Read TLS cert fingerprint from cert file
3. Read `auto_agent_auth_accepted` from GlobalConfig
4. Return `AuthOutcome` carrying all three fields

---

## 7. Real `AuthEngine::ensure_self_signed_tls`

File: `src/engine/auth/mod.rs`. Currently returns `EngineError::NotImplemented("self-signed TLS material is implemented in a later WI")`.

Replace with real `rcgen`-based self-signed cert generation:

- Cert SAN: includes the supplied `bind_ip` (typically `127.0.0.1`) and `localhost`
- Validity: 10 years (matches old-amux)
- Subject CN: `amux-headless-<short-hash-of-bind-ip>`
- Persist to: `<HOME>/.amux/headless/tls/cert.pem` + `<HOME>/.amux/headless/tls/key.pem` (mode 0600 for the key)
- Idempotent: if both files exist and the cert's SAN matches `bind_ip`, return existing material without regenerating
- When `bind_ip` changes between runs: regenerate the cert and emit `UserMessage::warning("TLS cert regenerated for new bind IP — pinned remote clients will need to re-pin")`
- Fingerprint stability: SHA-256 of DER-encoded cert. Surface as `TlsMaterial::fingerprint` so the remote command can pin against it

### `AuthEngine::refresh_api_key()`

Complete the existing partial implementation:

1. Generate 32 random bytes, hex-encode — that's the plaintext key
2. SHA-256 hash it; persist the hash to `<HOME>/.amux/headless/api-key.hash` (mode 0600)
3. Return `RefreshedApiKey { plaintext, hash, fingerprint: short_hex(hash[..8]) }`

Path resolution helpers live in `src/data/fs/headless_paths.rs` (Layer 0). Cryptographic logic lives in `src/engine/auth/mod.rs` (Layer 1).

---

## 8. Test layout and philosophy

**Only Layer 3 headless unit tests + Layer 1 auth-engine unit tests + the route-parity assertion guard.** The full parity test suite (real-loopback HTTP tests, real-rustls cert tests) happens in WI 0073. **Do not create files under `tests/` in this work item.**

### Unit tests to include

**`src/engine/auth/mod.rs`**:
- `ensure_self_signed_tls` happy path: cert + key written to correct paths, fingerprint is a valid hex string
- Idempotency: second call returns same cert (byte-identical)
- SAN mismatch: changing bind_ip regenerates cert
- `refresh_api_key`: hash file written with mode 0600, plaintext returned, hash is SHA-256 of plaintext

**`src/frontend/headless/routes.rs`**:
- Route-parity assertion: `const EXPECTED_ROUTES: &[(&str, &str)]` table copied verbatim from `oldsrc/commands/headless/server.rs::build_router`, asserted against the new `build_router` registrations

**`src/frontend/headless/command_frontend.rs`**:
- `parse_command_path` data-table test covering every catalogue command + nested subcommand
- Flag parsing from args vector matches clap-parsed equivalent

**`src/frontend/headless/auth.rs`**:
- Token mode: good key passes, bad key rejects with 401
- Disabled mode: `X-Amux-Auth: disabled` header emitted
- TLS-required mode: rejects non-loopback bind without TLS

**`src/frontend/headless/container_log.rs`**:
- SSE wire format snapshot: against frozen fixture, line-per-`data:`, `[amux:done]` sentinel

**`src/command/commands/headless.rs`**:
- `Start` honors flags: port, background, refresh-key short-circuit, dangerously-skip-auth
- `Kill` removes PID file after signal
- `Status` HTTP-probes correctly

**`src/command/commands/remote.rs`**:
- Address resolution precedence: flag > env > config
- API-key resolution precedence with canonicalized-default-addr edge case
- Session picker prompt path
- `--follow` SSE consumer behavior
- HTTP timeout configuration

**`src/data/headless_db.rs`**:
- Session CRUD round-trips
- Command CRUD round-trips
- Schema compatibility with legacy fixture DB

### Build & CI

- `cargo build --release` produces a single statically-linked `amux`
- `cargo test` passes including the new colocated tests
- `cargo clippy --all-targets -- -D warnings` passes
- `make all`, `make install`, `make test` work

---

## 9. Manual sign-off checklist (gating WI 0073)

The PR description MUST include:

- A confirmation that `amux headless start` was run on a real machine, the server bound, every documented endpoint received a real `curl` invocation (including `--refresh-key` mode and `--background` mode), and responses were wire-compatible with pre-refactor.
- A confirmation that `amux remote run -- exec prompt "hi" --yolo` was run against a real headless server and the trailing args reached the remote without "unknown flag" errors.
- A confirmation that TLS material was generated, the cert SAN was correct, and a `curl --cacert <cert>` round-trip succeeded.
- A confirmation that `amux auth --refresh-key` printed the legacy banner exactly.
- A table of every documented headless endpoint marked PASS / MINOR-DRIFT (one-sentence justification) / REGRESSION (block).
- A confirmation that `oldsrc/` was NOT touched (other than possibly `oldsrc/README.md`).

A REGRESSION blocks the PR.

---

## What must NOT happen in this work item

- **No business logic in `src/frontend/headless/`.** If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2.
- **No deletion of `oldsrc/`.** That is WI 0073.
- **No changes to the headless HTTP API surface.** No route paths, no HTTP methods, no request body fields, no response body fields.
- **No edits inside `oldsrc/`** other than possibly `oldsrc/README.md`.
- **No new commands, no new flags, no new user-visible behavior.** This work item closes the headless gap; it does not add to the surface.
- **No tests under `tests/`.** WI 0073 owns that tree.
- **No CLI or TUI changes** — those landed in WI 0070/0071. If a regression is discovered, fix it as a one-line correction with a test, but do NOT bundle a TUI feature here.
- **No Layer 1 changes outside of `AuthEngine`** — every gap discovered is logged in `aspec/review-notes/0072-followups.md` for WI 0073, unless the gap blocks headless parity.

---

## Edge Case Considerations

- **PID file race on start**: two simultaneous `amux headless start` invocations — the second sees the first's PID file. If the PID is alive AND is the amux server, exit with `CommandError::HeadlessAlreadyRunning { pid }`. If the PID is dead (stale file), clean up and proceed.
- **`--background` on Windows**: Unix `fork`+`setsid` doesn't apply; use `CREATE_NEW_PROCESS_GROUP` and `CreateProcessW`. Match old-amux semantics: foreground process exits cleanly after spawning the daemon.
- **TLS cert SAN mismatch on second run**: when `bind_ip` changes, regenerate cert and warn. See §7.
- **API key hash file missing on serve start**: when `--dangerously-skip-auth` is NOT set and hash file doesn't exist, error with `CommandError::HeadlessAuthMissing` and hint to run `amux auth --refresh-key`.
- **SSE backpressure**: clients that read slowly — write to SSE channel with bounded queue (size 256); on overflow, drop oldest and emit `amux-message: "warning: stream backpressure — some output dropped"`. Match old-amux semantics if it had one; else ASK THE DEVELOPER.
- **WebSocket support**: check `oldsrc/commands/headless/server.rs` for which routes use WS vs SSE; preserve verbatim.
- **HTTP timeouts on remote run**: connect=10s, read=600s for non-follow; follow disables read timeout (or sets to 24h). Match `oldsrc/commands/remote.rs::DEFAULT_TIMEOUTS`.
- **`--api-key` precedence with default-addr canonicalization**: `https://example.com:443` and `https://example.com/` canonicalize to the same address. Preserve.
- **Detached HEAD on remote session start**: emit `UserMessage::warning("detached HEAD — proceeding")` and continue.
- **Long-running command with --follow disconnect**: command continues running on server. Confirm against old behavior.
- **`auto_agent_auth_accepted` first-run consent**: `None` → prompt → persist; `Some(true)` → silent inject; `Some(false)` → no inject. Preserve.
- **Port collision**: if the configured port is already in use, error with structured message including the port number and a suggestion to use `--port`.
- **Workdir allowlist enforcement**: CLI `--workdirs` merges with `GlobalConfig::headless.workdirs`; non-existent paths rejected with structured errors; commands targeting non-allowed dirs rejected with 403.
- **SQLite schema migration**: `HeadlessDb::migrate()` must handle both fresh creation and upgrade from pre-refactor schema without data loss.

---

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- The CLI frontend in `src/frontend/cli/` is the reference implementation for trait patterns.
- `oldsrc/commands/headless/` is the behavioral reference for what to reproduce.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is WI 0073.
- Do not introduce business logic in `src/frontend/headless/`.
- Do not introduce upward calls — use traits.
- The PR description MUST link to this work item, MUST include the headless parity smoke-test checklist, and MUST list every developer-clarification question raised.
- After this work item lands, the next agent picks up `0073-grand-architecture-finalize.md`.
