# Work Item: Feature

Title: Headless Mode Part 4
Issue: issuelink

## Summary:
- Add cryptographic API key authentication to the headless server: a random key is generated at startup, only its hash (via `ring`) is persisted, and an auth middleware wraps every HTTP handler
- Add `--api-key` flag, `AMUX_API_KEY` env var, and `remote.defaultAPIKey` global config field to all `remote` subcommands and TUI remote operations, with correct precedence and a security constraint preventing config-sourced keys from leaking to non-default hosts
- Fix the TUI session picker dialog to have a dynamic width that accommodates long session IDs and paths, truncating session IDs only when needed
- Filter session picker dialogs to show only non-closed sessions; add a server-side query param for status filtering
- Prevent commands from being dispatched to closed sessions (already enforced); purge sessions closed >24h ago at server startup
- Increase the HTTP client read timeout from 60s to 10m and surface a clear timeout error to the user


## User Stories

### User Story 1:
As a: security-conscious operator running a headless amux server exposed to a network

I want to: have the server automatically generate a cryptographic API key on first start and require that key on every request, storing only its hash on disk so the plaintext key is never persisted anywhere

So I can: trust that only clients I have shared the key with can submit commands or read session data, without relying on network-level access control alone

### User Story 2:
As a: developer running `amux remote run` or managing remote sessions from the CLI or TUI

I want to: pass my API key once via `--api-key`, `AMUX_API_KEY`, or `remote.defaultAPIKey` in config and have every remote request automatically authenticated

So I can: drive a secured headless server without having to manually include the key in every command, while still being protected from accidentally sending my stored key to an unexpected host

### User Story 3:
As a: developer using TUI session pickers or long-running `remote run --follow` commands

I want to: see a session picker that is wide enough to display full paths and IDs without clipping, contains only open sessions, and have log-streaming commands time out gracefully after 10 minutes with a helpful error message

So I can: work with the remote host efficiently without being confused by truncated UI elements, stale closed sessions cluttering the picker, or cryptic network errors


## Implementation Details:

### 1. Server-side authentication (`src/commands/headless/`)

#### 1a. Key generation and storage (`src/commands/headless/auth.rs`) — new file

Add a new `auth.rs` module to the `headless` crate:

```rust
use anyhow::Result;
use ring::digest;
use std::path::Path;

/// File name within the headless root where the key hash is stored.
pub const KEY_HASH_FILE: &str = "api_key.hash";

/// Generate a cryptographically random 32-byte API key, encode as
/// lowercase hex (64 chars), and return it.  Uses `ring::rand::SecureRandom`.
pub fn generate_api_key() -> Result<String>;

/// Hash an API key using SHA-256 (via `ring::digest`) and return the
/// hex-encoded digest.  This is the same operation performed by both
/// the server (to store) and the middleware (to compare).
pub fn hash_api_key(key: &str) -> String;

/// Write the hex-encoded hash to `<headless_root>/api_key.hash`.
/// Creates the file with mode 0o600 on Unix.
pub fn write_key_hash(headless_root: &Path, hash: &str) -> Result<()>;

/// Read the hex-encoded hash from `<headless_root>/api_key.hash`.
/// Returns `None` if the file does not exist.
pub fn read_key_hash(headless_root: &Path) -> Result<Option<String>>;
```

**Startup key lifecycle** (`src/commands/headless/mod.rs`, in `run_start` before the logger is initialised):

1. If `--dangerously-skip-auth` is passed: skip all key steps, set `auth_mode = AuthMode::Disabled`.
2. Else if `--refresh-key` is passed: call `generate_api_key()`, `hash_api_key()`, `write_key_hash()`, print the plaintext key to stdout with a clear banner, set `auth_mode = AuthMode::Required(hash)`.
3. Else if `read_key_hash()` returns `Some(hash)`: use the existing hash, set `auth_mode = AuthMode::Required(hash)`. Do not print anything about the key.
4. Else (no hash exists): call `generate_api_key()`, `hash_api_key()`, `write_key_hash()`, print the plaintext key to stdout with a clear banner, set `auth_mode = AuthMode::Required(hash)`.

**stdout banner format** (the only place the plaintext key ever appears):
```
╔═══════════════════════════════════════════════════════════════════╗
║  amux headless API key (store this — it will not be shown again)  ║
║  <64-char hex key>                                                ║
╚═══════════════════════════════════════════════════════════════════╝
```

**Never write the key to the log file or any other location.**  Perform key creation/refresh before `init_logging()` to make accidental log-file capture structurally impossible.

#### 1b. Authentication middleware (`src/commands/headless/server.rs`)

Add an `AuthMode` enum to `AppState`:

```rust
pub enum AuthMode {
    /// All requests are accepted without checking credentials.
    Disabled,
    /// Every request must present a key whose SHA-256 hash matches this value.
    Required(String), // hex-encoded SHA-256 hash, loaded into memory at startup
}
```

Add `pub auth_mode: AuthMode` to `AppState`.

When `auth_mode` is `AuthMode::Required`, add a Tower middleware layer that intercepts **every request** before it reaches any handler.  Use `axum::middleware::from_fn_with_state` (or a custom `tower::Layer`) so the check applies globally to the entire router — do not add auth logic to individual handlers:

```rust
pub fn build_router(state: Arc<AppState>) -> Router {
    let router = Router::new()
        .route("/v1/status", get(handle_status))
        // ... all other routes ...
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    match &state.auth_mode {
        AuthMode::Disabled => router,
        AuthMode::Required(_) => router.layer(axum::middleware::from_fn_with_state(
            state,
            auth_middleware,
        )),
    }
}
```

**`auth_middleware` logic:**
1. Extract the `Authorization` header value; also accept the raw key without the `Bearer ` prefix (strip `Bearer ` if present, case-insensitively).
2. If the header is absent or empty: return HTTP 401 with JSON body `{"error": "API key required. Pass the key via the Authorization header (e.g. Authorization: Bearer <key>)."}`.
3. Hash the provided value with `hash_api_key()`.
4. Compare (constant-time) with the stored hash from `AppState.auth_mode`.  Use `ring::constant_time::verify_slices_are_equal` on the byte slices of the hex strings to prevent timing attacks.
5. If hashes differ: return HTTP 401 with JSON body `{"error": "Invalid API key."}`.
6. On success: call `next.run(request).await`.

The middleware must be applied as a single layer that covers **all routes**; it must not be added per-handler.

#### 1c. New CLI flags (`src/cli.rs`)

Add two new flags to `HeadlessAction::Start`:

```rust
/// Regenerate the API key: creates a new key, stores the new hash,
/// prints the new key to stdout, and discards the old one.
#[arg(long)]
refresh_key: bool,

/// Disable authentication for this execution even if a key hash exists on disk.
/// WARNING: any client can reach the server without credentials.
#[arg(long)]
dangerously_skip_auth: bool,
```

Update `src/commands/spec.rs` — `HEADLESS_START_FLAGS` — to include `refresh-key` and `dangerously-skip-auth`, and update the CLI/spec parity test.

Update `src/commands/headless/mod.rs` — `run_start` — to accept and act on both new flags.

#### 1d. Dependency (`Cargo.toml`)

Add `ring` to `[dependencies]`:

```toml
ring = "0.17"
```

`ring` provides `ring::rand::SecureRandom`, `ring::digest` (SHA-256), and `ring::constant_time::verify_slices_are_equal`.  Do not use `sha2` for this feature; `ring` is the single source of truth for all cryptographic operations in the headless auth flow.

---

### 2. Client-side authentication (`src/commands/remote.rs`, `src/config/mod.rs`, `src/cli.rs`)

#### 2a. Config additions (`src/config/mod.rs`)

Add `default_api_key` to `RemoteConfig`:

```rust
/// Default API key sent with every request to the default remote host.
/// Only used when the request target matches `defaultAddr`.
/// NEVER sent to any other host.
#[serde(rename = "defaultAPIKey", skip_serializing_if = "Option::is_none")]
pub default_api_key: Option<String>,
```

Add a config field definition to `ALL_FIELDS` in `src/commands/config.rs`:

```rust
ConfigFieldDef {
    key: "remote.defaultAPIKey",
    scope: FieldScope::GlobalOnly,
    hint: "API key for the default remote headless amux host (only sent to defaultAddr)",
    builtin_default: "(not set)",
    settable: true,
},
```

Add `config get/set` dot-path support in `src/commands/config.rs` alongside the existing `remote.*` arms.

#### 2b. Key resolution function (`src/commands/remote.rs`)

Add a new public function:

```rust
/// Resolve the API key to send with a request to `target_addr`.
///
/// Priority:
///   1. `--api-key` CLI flag (passed as `flag`)
///   2. `AMUX_API_KEY` environment variable
///   3. `remote.defaultAPIKey` from global config — BUT ONLY when
///      `target_addr` matches `remote.defaultAddr` exactly (after stripping
///      trailing slashes from both).  If the hosts differ, config key is ignored.
///
/// Returns `None` when no key is available (caller decides whether to error
/// or proceed without auth — e.g. server may have --dangerously-skip-auth).
pub fn resolve_api_key(flag: Option<&str>, target_addr: &str) -> Option<String>;
```

The host-match guard prevents a stored default key from being silently forwarded to an attacker-controlled host if the user changes `--remote-addr` or `AMUX_REMOTE_ADDR`.

#### 2c. HTTP client modification (`src/commands/remote.rs`)

Update all functions that call `make_client()` and issue HTTP requests (`run_remote_run`, `run_remote_session_start`, `run_remote_session_kill`, `fetch_sessions`, `stream_command_logs`) to accept an `api_key: Option<&str>` parameter and add it to outgoing requests:

```rust
if let Some(key) = api_key {
    request_builder = request_builder.header("Authorization", format!("Bearer {}", key));
}
```

Centralise this in a `build_request(client, method, url, api_key)` helper so the auth header is never omitted accidentally.

Also increase the read timeout:

```rust
fn make_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(600)) // 10 minutes
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))
}
```

When a `reqwest` error is a timeout (check `err.is_timeout()`), surface a tailored message:
```
Request timed out after 10 minutes. The remote command may still be running.
Check its status with: amux remote run ... (or query /v1/commands/<id> directly).
```

#### 2d. CLI flag additions (`src/cli.rs`)

Add `--api-key` to `RemoteAction::Run`, `RemoteSessionAction::Start`, and `RemoteSessionAction::Kill`:

```rust
/// API key for the remote headless amux host.
/// Overrides AMUX_API_KEY env var and remote.defaultAPIKey config.
#[arg(long)]
api_key: Option<String>,
```

Pass the flag value through the dispatch chain to `resolve_api_key()`.

Update `src/commands/spec.rs` — `REMOTE_RUN_FLAGS`, `REMOTE_SESSION_START_FLAGS`, `REMOTE_SESSION_KILL_FLAGS` — to include `api-key`, and update the CLI/spec parity tests.

#### 2e. TUI support (`src/tui/mod.rs`, `src/tui/state.rs`)

In `TuiRemoteConfig` (or the equivalent state that carries resolved remote params), add an `api_key: Option<String>` field populated by calling `resolve_api_key(None, &remote_addr)` at the point where `remote_addr` is resolved.  This resolved key flows through `PendingCommand::RemoteRun`, `PendingCommand::RemoteSessionStart`, and `PendingCommand::RemoteSessionKill` so that `launch_remote_*` functions can pass it to `run_remote_*` functions.

When constructing the `PendingCommand` variants in `execute_command`, resolve the API key immediately after resolving `remote_addr`:

```rust
let api_key = crate::commands::remote::resolve_api_key(None, &remote_addr);
```

Add `api_key: Option<String>` to all three `PendingCommand` variants and thread it through to the underlying `run_remote_*` call.

---

### 3. Misc fixes

#### 3.1. Dynamic-width session picker dialog (`src/tui/render.rs`)

Replace the fixed-width calculation in `draw_remote_picker`:

```rust
// Current (fixed):
let popup_width = 80u16.min(area.width.saturating_sub(4));
```

With a dynamic calculation:

```rust
// New (dynamic):
let max_allowed_width = (area.width as usize * 80 / 100).max(20); // 80% of window
let content_width = items.iter()
    .map(|s| s.chars().count())
    .max()
    .unwrap_or(20)
    .max(title.chars().count())
    + 4; // 2 chars border + 2 chars padding each side

let popup_width = content_width.min(max_allowed_width) as u16;
```

**Session ID truncation:** When formatting the session picker row strings in `draw_tabs` / the `RemoteSessionPicker` render branch, truncate the session ID portion — not the workdir — if the total row width would exceed `max_allowed_width`:

```rust
// In the render branch for RemoteSessionPicker:
let max_id_chars = max_allowed_width.saturating_sub(workdir.chars().count() + 6);
let id_display = if s.id.chars().count() > max_id_chars && max_id_chars > 3 {
    format!("{}…", &s.id[..max_id_chars.saturating_sub(1)])
} else {
    s.id.clone()
};
format!("{}  ({})", id_display, s.workdir)
```

Apply the same dynamic width logic to `RemoteSavedDirPicker` and `RemoteSessionKillPicker` (they already share `draw_remote_picker`).

#### 3.2. Filter sessions to non-closed only (`src/commands/remote.rs`, `src/commands/headless/server.rs`)

**Server side:** Add an optional `status` query parameter to `GET /v1/sessions`:

```
GET /v1/sessions?status=active
```

In `handle_list_sessions`, extract the query param and filter the DB result set when `status=active` is requested.  Use `db::list_sessions_by_status(conn, Some("active"))` (new overload or existing function extended with an `Option<&str>` filter param).

**Client side:** In `fetch_sessions` in `src/commands/remote.rs`, append `?status=active` to the request URL so that session picker dialogs never show closed sessions:

```rust
let url = format!("{}/v1/sessions?status=active", remote_addr.trim_end_matches('/'));
```

The existing `GET /v1/sessions` endpoint (without `?status`) continues to return all sessions (active and closed) for full auditability — only the TUI picker client sends the filter.

#### 3.3. Reject commands on closed sessions; startup cleanup (`src/commands/headless/server.rs`, `src/commands/headless/db.rs`)

**Closed-session command rejection** (already enforced in WI 0057 — verify and document):  `handle_create_command` already returns HTTP 404 when the session is closed.  Confirm this path returns a clear message and is tested.

**Startup cleanup** (`src/commands/headless/mod.rs`, called once from `run_start` after the DB is opened):

Add a function `db::delete_closed_sessions_older_than(conn, hours: u64) -> Result<usize>` that executes:

```sql
DELETE FROM sessions
WHERE status = 'closed'
  AND closed_at IS NOT NULL
  AND closed_at < datetime('now', '-24 hours');
```

Also cascade-delete or nullify associated `commands` rows for the deleted sessions (or rely on the file-system directories for storage, which are not deleted — just the DB rows).  Log the count of deleted sessions at `INFO` level.

Call this once at startup:

```rust
let deleted = db::delete_closed_sessions_older_than(&db, 24)?;
if deleted > 0 {
    tracing::info!(deleted_sessions = deleted, "Purged closed sessions older than 24h");
}
```
Ensure to print a log line with the standard logger for each session that is deleted by the startup cleanup routine and make it clear why they are being deleted (something like "running stale closed session cleanup" and then "deleted stale session {} and {n} linked command records")


#### 3.4. HTTP read timeout increase and helpful error (`src/commands/remote.rs`)

The read timeout change is specified in section 2c above.  Additionally, wrap all `reqwest` error sites with a timeout check:

```rust
fn map_reqwest_error(err: reqwest::Error, addr: &str) -> anyhow::Error {
    if err.is_timeout() {
        anyhow::anyhow!(
            "Request to {} timed out after 10 minutes.\n\
             The remote command may still be running on the server.\n\
             Check its status with: curl {}/v1/commands/<id>",
            addr, id
        )
    } else if err.is_connect() {
        anyhow::anyhow!("Could not connect to {}: {}", addr, err)
    } else {
        anyhow::anyhow!("HTTP request to {} failed: {}", addr, err)
    }
}
```

Apply `map_reqwest_error` consistently in all `.await?` call sites in the remote module.


## Edge Case Considerations:

### Authentication

- **First start (no hash on disk):** Key is generated, hash written to `api_key.hash`, plaintext printed to stdout once. Any client that missed the output must use `--refresh-key` to cycle the key.
- **`--refresh-key` replaces existing hash:** The old key is invalidated immediately on startup. Existing clients using the old key will receive HTTP 401 and must be updated.
- **`--dangerously-skip-auth` with existing hash on disk:** Auth is disabled only for the current process lifetime. The hash file remains on disk; the next normal startup will re-enable auth with the stored hash.
- **Concurrent requests during startup:** Key setup completes before the server binds and starts accepting connections, so no request can arrive before the middleware is fully installed.
- **Timing attack prevention:** Use `ring::constant_time::verify_slices_are_equal` for hash comparison, not `==` on strings.
- **Log file safety:** Logging is initialised *after* key generation. The banner is printed to stdout via `println!`, never via `tracing`. This guarantees the key cannot appear in `amux.log` even if the log subscriber is configured before startup completes.
- **Key hash file permissions:** On Unix, write with mode `0o600` (owner read/write only) to prevent other users on the same machine from reading the hash.

### Client authentication

- **Config key sent to wrong host:** `resolve_api_key` compares `target_addr` to `remote.defaultAddr` after stripping trailing slashes from both. Only an exact match (scheme + host + port + path prefix) allows the config key to be used. Any other host — whether from `--remote-addr` or `AMUX_REMOTE_ADDR` — gets no config key.
- **No key configured, server requires auth:** The response body from the server includes a clear instruction about which header to use. The client should surface this body text verbatim rather than a generic HTTP error.
- **API key in environment variable on shared machines:** Document in help text that `AMUX_API_KEY` is visible in `/proc/<pid>/environ` on Linux; prefer `--api-key` piped from a secrets manager or the config file with restricted permissions.

### Dynamic dialog width

- **Very long paths/IDs that exceed 80% of terminal width:** The session ID is truncated (with `…`) before the workdir, preserving the workdir which is more recognisable. Minimum popup width is 20 characters.
- **Very narrow terminals (< 25 columns):** The dialog falls back to minimum width and may clip content; this is acceptable as the TUI is generally unusable below ~80 columns.
- **Title longer than content:** `content_width` takes the maximum of item width and title width, so the border is always wide enough for the title.

### Session filtering and cleanup

- **`fetch_sessions` with `?status=active` returns empty list:** TUI picker shows the "no active sessions" message and both Enter and Esc close the dialog without action.
- **Session transitions to closed between fetch and command dispatch:** The server enforces closed-session rejection at `POST /v1/commands` time regardless of client-side filtering. The client receives HTTP 404 and surfaces a clear error.
- **Startup cleanup of sessions closed exactly 24h ago:** The `datetime('now', '-24 hours')` boundary in SQLite is exclusive (`<`), so sessions closed exactly 24 hours ago are not deleted until the next second boundary passes.
- **Commands rows for deleted sessions:** Delete commands rows whose `session_id` is no longer in the sessions table (either via `ON DELETE CASCADE` in the schema or an explicit cleanup step in `delete_closed_sessions_older_than`). On-disk log files are not deleted — they remain in `~/.amux/headless/sessions/<uuid>/` for audit purposes.

### Timeout handling

- **SSE streaming (`--follow`) with long-running commands:** The 10-minute read timeout applies to the SSE connection as well. If the command runs longer than 10 minutes, the client will time out and display the helpful message. The command continues running on the server; the user can reconnect by running `remote run` again without `--follow` to check status or re-attach. Any activity or SSE data recieved from the server should re-set the 10m timeout (amux workflows may take hours, for example), but if the server is completely silent for 10+mins, it's OK for the client to disconnect.
- **Short non-streaming requests:** The 10-minute timeout is generous for short requests. If a non-streaming request takes that long, it indicates a server hang; the timeout error message correctly directs the user to check command status on the server.


## Test Considerations:

### Unit tests — `src/commands/headless/auth.rs`
- `generate_api_key()` produces a 64-character lowercase hex string.
- Two successive calls to `generate_api_key()` return different values.
- `hash_api_key("abc")` returns the SHA-256 hex digest of `"abc"` (compare against a known test vector).
- `hash_api_key(key) == hash_api_key(key)` for any key (deterministic).
- `write_key_hash` + `read_key_hash` round-trips correctly in a `TempDir`.
- `read_key_hash` returns `None` when the file does not exist.
- On Unix: `write_key_hash` creates the file with mode `0o600`.

### Unit tests — `src/commands/headless/server.rs` (auth middleware)
- Request with correct key passes through and receives a 200 response.
- Request with no `Authorization` header receives HTTP 401 with the expected JSON error body.
- Request with wrong key receives HTTP 401.
- Request with `Bearer <key>` (with prefix) is accepted.
- Request with bare `<key>` (without prefix) is accepted.
- When `AuthMode::Disabled`, all requests pass regardless of the `Authorization` header.

### Unit tests — `src/cli.rs`
- `amux headless start --refresh-key` parses `refresh_key = true`.
- `amux headless start --dangerously-skip-auth` parses `dangerously_skip_auth = true`.
- Both flags can be combined with existing flags (`--port`, `--workdirs`, `--background`).
- `amux remote run --api-key abc123 execute prompt hello` parses `api_key = Some("abc123")`.
- `amux remote session start --api-key abc123 /workspace/proj` parses correctly.
- `amux remote session kill --api-key abc123 <session-id>` parses correctly.
- CLI/spec parity test for `headless start` now includes `refresh-key` and `dangerously-skip-auth`.
- CLI/spec parity tests for `remote run`, `remote session start`, `remote session kill` include `api-key`.

### Unit tests — `src/commands/remote.rs`
- `resolve_api_key(Some("flag-key"), "http://host:9876")` returns `Some("flag-key")` regardless of config/env.
- `resolve_api_key(None, "http://host:9876")` returns the env var value when `AMUX_API_KEY` is set.
- `resolve_api_key(None, "http://host:9876")` returns the config value when `target_addr` matches `remote.defaultAddr` (trailing-slash normalisation verified).
- `resolve_api_key(None, "http://other-host:9876")` returns `None` even when `remote.defaultAPIKey` and `remote.defaultAddr` are both set but hosts differ.
- `map_reqwest_error` with a timeout error returns a message containing "timed out after 10 minutes".
- `make_client()` read timeout is 600 seconds (verify via `reqwest::Client` builder inspection or a mock).

### Unit tests — `src/config/mod.rs`
- `RemoteConfig` with `defaultAPIKey` round-trips through JSON.
- `GlobalConfig` serialises `remote.defaultAPIKey` as `"defaultAPIKey"` (camelCase).
- `config get remote.defaultAPIKey` and `config set remote.defaultAPIKey` work correctly.

### Unit tests — `src/commands/headless/db.rs`
- `delete_closed_sessions_older_than(conn, 24)` deletes sessions closed more than 24h ago and returns the correct count.
- Sessions closed exactly 24h ago are not deleted (boundary is exclusive).
- Active sessions are never deleted by this function.
- Associated commands rows for deleted sessions are also removed.

### Unit tests — `src/commands/headless/server.rs` (session filtering)
- `GET /v1/sessions?status=active` returns only sessions with `status = "active"`.
- `GET /v1/sessions` (no query param) returns all sessions.
- `GET /v1/sessions?status=closed` returns only closed sessions.

### Unit tests — `src/tui/render.rs` (dynamic width)
- `draw_remote_picker` with a single 120-character item on an 80-column terminal produces `popup_width <= 64` (80% of 80).
- `draw_remote_picker` with a 30-character item on a 200-column terminal produces `popup_width >= 34` (fits content).
- Session ID is truncated with `…` when the full row would exceed 80% of terminal width; workdir is preserved.

### Unit tests — `src/commands/remote.rs` (host-match guard — added in parity review)
- `resolve_api_key(None, "http://other-host:9876")` returns `None` even when both `remote.defaultAPIKey`
  and `remote.defaultAddr` are set but the hosts differ (guards against key leakage to unexpected hosts).
- `resolve_api_key(None, "http://default-host:9876/")` (trailing slash) matches
  `remote.defaultAddr = "http://default-host:9876"` and returns the config key
  (trailing-slash normalisation).
- `resolve_api_key(None, "HTTP://DEFAULT-HOST:9876")` (uppercase scheme/host) matches
  `remote.defaultAddr = "http://default-host:9876"` and returns the config key
  (case-insensitive normalisation).

### Unit tests — `src/commands/headless/server.rs` (auth middleware — added in parity review)
- Request with correct bare key (no `Bearer ` prefix) is accepted with HTTP 200.
- Request with `Bearer <key>` (mixed-case `Bearer`) is accepted.
- Request with absent `Authorization` header returns HTTP 401 with JSON body containing
  "API key required" and instructions to use the `Authorization` header.
- Request with wrong key returns HTTP 401 with body `{"error": "Invalid API key."}`.
- Constant-time path: two requests — one with the correct key and one with a key that
  differs in the last character — both complete without timing-related error; neither
  short-circuits at the first differing byte in a way observable by the test.

### Integration tests
- Start server without a hash file; verify `api_key.hash` is created and the key printed to stdout passes auth.
- Start server with `--refresh-key`; verify old hash is replaced and the new key is printed.
- Start server with `--dangerously-skip-auth`; verify requests without `Authorization` are accepted.
- `fetch_sessions` with `?status=active` against a server that has both active and closed sessions returns only active ones.
- `delete_closed_sessions_older_than` called in a test with a SQLite DB populated with old and recent closed sessions deletes only old ones.
- Full auth round-trip: start server, resolve key from `AMUX_API_KEY`, call `fetch_sessions`, assert success.
- Full auth round-trip with bare key: server started normally, client sends key without `Bearer ` prefix,
  `fetch_sessions` succeeds with HTTP 200.
- Config-key host-guard integration: server at `http://host-A:9876`, config `remote.defaultAddr = "http://host-B:9876"`,
  config `remote.defaultAPIKey` set — verify `resolve_api_key(None, "http://host-A:9876")` returns `None`
  and the request is rejected with 401 (key not forwarded to non-default host).


## Codebase Integration:
- Add `pub mod auth;` to `src/commands/headless/mod.rs` alongside the existing `db`, `server`, `process`, and `logging` modules.
- `AuthMode` is defined in `server.rs` (where `AppState` lives) and references the hash string loaded by `auth.rs` at startup. `run_start` in `mod.rs` bridges the two: it calls `auth.rs` functions to produce the hash, then passes it into `AppState`.
- The `ring` crate is the only cryptographic dependency needed for this work item. Do not use `sha2` for auth hashing even though it is already in `Cargo.toml`; keep crypto consolidated in `ring` for the headless auth path.
- `HeadlessAction::Start` gains two new fields (`refresh_key: bool`, `dangerously_skip_auth: bool`). The exhaustive pattern match in `src/commands/mod.rs` that destructures this variant must be updated.
- `RemoteAction::Run`, `RemoteSessionAction::Start`, and `RemoteSessionAction::Kill` each gain an `api_key: Option<String>` field. All match arms and dispatch paths that destructure these variants must be updated — including the TUI `execute_command` parser and `PendingCommand` variants in `src/tui/state.rs`.
- `HEADLESS_START_FLAGS` in `src/commands/spec.rs` must add `refresh-key` (boolean) and `dangerously-skip-auth` (boolean). `REMOTE_RUN_FLAGS`, `REMOTE_SESSION_START_FLAGS`, and `REMOTE_SESSION_KILL_FLAGS` must add `api-key` (value, `value_name: "KEY"`). The CLI/spec parity tests in `src/cli.rs` enforce this at compile time.
- `draw_remote_picker` in `src/tui/render.rs` is the single function to update for the dynamic-width fix; all three picker dialog types (`RemoteSessionPicker`, `RemoteSessionKillPicker`, `RemoteSavedDirPicker`) call through it. Session ID truncation logic lives in the render branches that format row strings before passing them to `draw_remote_picker`.
- `db::list_sessions` (or equivalent) in `src/commands/headless/db.rs` gains an optional `status: Option<&str>` filter parameter. The HTTP handler in `server.rs` extracts the `status` query param using `axum::extract::Query`.
- `db::delete_closed_sessions_older_than` is a new function in `src/commands/headless/db.rs`, called once from `run_start` after the DB connection is established.
- Follow established conventions: `#[serde(rename = "defaultAPIKey", skip_serializing_if = "Option::is_none")]` on the new config field; resolution priority documented in a doc comment on `resolve_api_key`; `AMUX_API_KEY` env var checked with `std::env::var`.
