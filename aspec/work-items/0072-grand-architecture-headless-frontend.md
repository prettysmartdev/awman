# Work Item: Task

Title: grand architecture refactor — Headless frontend + headless/remote/auth command bodies + TLS engine
Issue: n/a — seventh-of-eight work item implementing `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item builds the headless server frontend AND the still-stubbed Layer 2 command bodies that exist only to talk to the headless server. The implementing agent **MUST** read:

- `aspec/architecture/2026-grand-architecture.md` end-to-end.
- `0066-…` through `0069-…` (foundation work items).
- `0070-grand-architecture-layer-1-2-completion-and-cli.md` (Layer 1/2 + CLI completion — this WI's prerequisite).
- `0071-grand-architecture-tui-frontend.md` (TUI frontend — also a prerequisite, since some headless dialog defaults reference TUI dialog enums).
- `0069-…` §3 + §7u (the original headless section and the headless-defaults addendum — these remain authoritative for HTTP API specifics).
- `oldsrc/commands/headless/server.rs` end-to-end (the legacy headless server; the new server's HTTP API surface MUST be wire-identical).
- `oldsrc/commands/remote.rs` and `oldsrc/commands/auth.rs` (the legacy command bodies being ported).

The four tenets, again:

1. **Frontends contain NO business logic.**
2. **Lower layers never call upward.** Use traits.
3. **Typed objects over `pub fn`.**
4. **When uncertain, ASK THE DEVELOPER.**

The companion work items are:

- `0066-grand-architecture-foundation-and-layer-0-data.md` (merged)
- `0067-grand-architecture-layer-1-engines.md` (merged)
- `0068-grand-architecture-layer-2-command-and-dispatch.md` (merged)
- `0069-grand-architecture-layer-3-frontends-and-binary.md` (merged)
- `0070-grand-architecture-layer-1-2-completion-and-cli.md` (must be merged)
- `0071-grand-architecture-tui-frontend.md` (must be merged)
- `0073-grand-architecture-finalize-and-remove-oldsrc.md`

## Scope

Three deliverables:

1. **`src/frontend/headless/`** — full headless HTTP server per `0069-…` §3 + §7u. Wire-identical to `oldsrc/commands/headless/server.rs`; only internal change is that `POST /v1/commands` dispatches through `Dispatch` instead of spawning a child `amux` process.
2. **Real Layer 2 command bodies** for `headless start/kill/logs/status`, `remote run`, `remote session start`, `remote session kill`, and the headless-side persistence half of `auth`. These are stubbed in 0068/0070 because they only become meaningful once the headless server exists.
3. **Real `AuthEngine::ensure_self_signed_tls`** — currently `EngineError::NotImplemented`. Real `rcgen` (or equivalent) self-signed cert generation, fingerprint stability per `0067-…` §9a.

After this work item, `amux headless start` boots a real HTTP server that serves the legacy API, `amux headless kill/logs/status` manage it, `amux remote *` talk to it from another host, and `amux auth` round-trips through the global config persistence layer cleanly.

## Implementation Details

### 1. `src/frontend/headless/` — files and structure

Per `0069-…` §3 + §7u, build these files:

- `mod.rs` — entry point: `pub async fn serve(config: HeadlessServeConfig, engines: Engines, session_manager: Arc<RwLock<SessionManager>>) -> Result<(), HeadlessError>`. **Layer 2 cannot call `serve` directly** — that would be an upward call. The headless `start` command (Layer 2) accepts a `HeadlessStartCommandFrontend` trait at instantiation; the CLI frontend's impl calls `crate::frontend::headless::serve(...)`. Peer call within Layer 3, allowed.
- `routes.rs` — registers the **same HTTP routes as `oldsrc/commands/headless/server.rs::build_router`**, verbatim. Route list is fixed; not derived from `CommandCatalogue`. Per `0069-…` §3, the routes are: `GET /v1/status`, `GET /v1/workdirs`, `GET /v1/sessions`, `POST /v1/sessions`, `GET /v1/sessions/:id`, `DELETE /v1/sessions/:id`, `POST /v1/commands`, `GET /v1/commands/:id`, `GET /v1/commands/:id/logs`, `GET /v1/commands/:id/logs/stream`, `GET /v1/workflows/:command_id`.
- `command_frontend.rs` — `HeadlessCommandFrontend` implementing `CommandFrontend`. Constructed from `CreateCommandRequest { subcommand: String, args: Vec<String> }`. Provides `parse_command_path(&self) -> Result<CommandPath, HeadlessError>`. Implements `CommandFrontend::get_flag` by parsing the remaining `args` against the command's known flags. For interactive Q&A it returns the §7u defaults; each MAY be overridden by request body parameters.
- `container_log.rs` — `HeadlessContainerFrontend` implementing `ContainerFrontend`. Writes container stdout/stderr to the command's `output.log` file — same path and format as the old-amux `execute_command` function. The `GET /v1/commands/:id/logs/stream` SSE endpoint streams from this file, line-per-`data:` event, terminated by `[amux:done]`. **Wire format byte-identical to old-amux.**
- `workflow_state.rs` — `HeadlessWorkflowFrontend` implementing `WorkflowFrontend`. Writes workflow state to `workflow.state.json` in the command directory — same path and format as old-amux. The `GET /v1/workflows/:command_id` endpoint reads from this file; JSON schema identical to old-amux.
- `user_message.rs` — `HeadlessUserMessageSink` implementing `UserMessageSink`. Emits each message as an SSE event of type `amux-message` with `{ "level": "info"|"warning"|"error"|"success", "text": "..." }`. `replay_queued` is a no-op (messages are streamed live).
- `worktree_lifecycle_frontend.rs` — `HeadlessWorktreeLifecycleFrontend` implementing `WorktreeLifecycleFrontend`. Uses request-parameter defaults for all decisions per §7u. Reports stream as `amux-message` SSE events. ASK THE DEVELOPER whether to expose Q&A decisions as separate API endpoints or as upfront request parameters.
- `auth.rs` — TLS + API-key middleware. Pure plumbing; cryptographic logic is in `AuthEngine` (Layer 1).
- `errors.rs` — translates `CommandError` etc. into HTTP status codes + JSON error bodies.
- `defaults.rs` — every safe non-interactive default per `0069-…` §7u as named constants.

The `POST /v1/commands` handler replaces the child-process spawn with a Dispatch call. All surrounding logic (session validation, concurrency guard, `x-amux-session` header, DB inserts, command directory creation, 202 Accepted response) is copied verbatim from `oldsrc/commands/headless/server.rs::handle_create_command` and `execute_command`; only the body of `execute_command` changes.

`CreateCommandRequest`, `CreateCommandResponse`, `SessionResponse`, `CommandResponse`, `StatusResponse`, and `ErrorResponse` — all Serde shapes are **identical to `oldsrc/commands/headless/server.rs`**. Do not rename fields, change types, or add/remove fields.

The grand architecture document explicitly forbids the server from "just calling the CLI": the headless frontend talks to `Dispatch` directly, never spawns a child `amux` process.

### 2. Real Layer 2 command bodies — headless

Files: `src/command/commands/headless.rs`. Currently `let _ = self.engines; HeadlessOutcome::*`.

The headless command surface is four subcommands plus the existing flag set:

- **`HeadlessSubcommand::Start { port, workdirs, background, refresh_key, dangerously_skip_auth }`** — port `oldsrc/commands/headless/mod.rs::run_start`:
  - Resolve effective `HeadlessServeConfig` from flags + `GlobalConfig::headless`.
  - When `--refresh-key`, call `AuthEngine::refresh_api_key()` which generates a new key, persists its hash to `<HOME>/.amux/headless/api-key.hash`, prints the plaintext key to stderr in the legacy banner format (verbatim from `oldsrc/commands/headless/server.rs::print_refresh_key_banner`), and returns. Do NOT proceed to serve in this mode (legacy behavior).
  - When `--background`, daemonize via `oldsrc/commands/headless/process.rs::spawn_background` (port verbatim — fork/setsid + nohup pattern). The foreground process exits cleanly after writing the PID file at `<HOME>/.amux/headless/amux.pid`.
  - When foreground, call `frontend.serve_until_shutdown(config)` (the per-command frontend trait method that the CLI's impl wires to `crate::frontend::headless::serve(...)`). Block until shutdown signal (SIGINT, SIGTERM).
  - On shutdown, remove the PID file via `HeadlessLifecycle::clear_pid()` (Layer 2 helper introduced in 0068 §6.4).
  - Return `HeadlessStartOutcome { bound_addr, refresh_key_printed, background }`.
- **`HeadlessSubcommand::Kill`** — port `oldsrc/commands/headless/mod.rs::run_kill`:
  - Read PID from `<HOME>/.amux/headless/amux.pid`. Stale-PID detection: if the PID's process is not the amux server (per `oldsrc/commands/headless/process.rs::pid_is_amux`), surface `CommandError::HeadlessNotRunning` and clean up the stale file.
  - Send SIGTERM; wait up to 5s; SIGKILL if still alive.
  - Remove PID file.
  - Return `HeadlessKillOutcome { pid, killed }`.
- **`HeadlessSubcommand::Logs`** — port `oldsrc/commands/headless/mod.rs::run_logs`:
  - Stream `<HOME>/.amux/headless/amux.log` to the supplied `UserMessageSink` (or stdout via the CLI's frontend impl). Tail behavior: the legacy command does NOT tail; it cats the file once and exits. Preserve.
  - Return `HeadlessLogsOutcome { lines_printed }`.
- **`HeadlessSubcommand::Status`** — port `oldsrc/commands/headless/mod.rs::run_status`:
  - Check PID file → process exists → reachable on `127.0.0.1:<port>` via a quick HTTP probe (`GET /v1/status`).
  - Return `HeadlessStatusOutcome { running, pid, bound_addr, version }` (last two `Option`).

The PID file lifecycle helpers move from `oldsrc/commands/headless/process.rs` to `src/data/headless_paths.rs` (Layer 0). The "spawn background" helper is OS-specific; gate per-OS implementations on `cfg(unix)` / `cfg(windows)` and use `fork`+`setsid` on Unix, `CREATE_NEW_PROCESS_GROUP` on Windows (matches old-amux).

### 3. Real Layer 2 command bodies — remote

Files: `src/command/commands/remote.rs`, `src/command/commands/remote_client.rs`. Currently `let _ = self.engines; RemoteOutcome::*` and `RemoteClient::stream_command` returns `EngineError::NotImplemented`.

Three subcommands:

- **`RemoteSubcommand::Run { command, remote_addr, session, follow, api_key }`** — port `oldsrc/commands/remote.rs::run_remote_run`:
  - Resolve effective remote address: `--remote-addr` > env `AMUX_REMOTE_ADDR` > `GlobalConfig::remote.default_addr`. Surface `CommandError::RemoteAddrMissing` when none.
  - Resolve effective API key: `--api-key` > env `AMUX_API_KEY` > `GlobalConfig::remote.default_api_key` *only when* the resolved address matches `GlobalConfig::remote.default_addr` after URL canonicalization. Per `0069-…` Edge Case "API-key resolution".
  - Resolve effective session: `--session` > prompt the user via the per-command frontend (CLI: prompt on stdin; TUI: open `RemoteSessionPicker` per `0069-…` §7q) if the server reports more than one. When server has zero sessions, error with `CommandError::RemoteSessionMissing` and a hint to run `amux remote session start`.
  - Build a `CreateCommandRequest { subcommand: command[0], args: command[1..] }`.
  - POST it via `RemoteClient::send_command` (already partially implemented; complete it). 202 Accepted → command_id.
  - When `--follow`, call `RemoteClient::stream_command(command_id)` which opens `GET /v1/commands/:id/logs/stream` (SSE), parses each `data:` line, and forwards through the supplied `UserMessageSink` (CLI: stderr; TUI: per-tab status log; headless: returns the stream as part of the response). Block until the `[amux:done]` sentinel.
  - When NOT `--follow`, return immediately with `RemoteRunOutcome { command_id, address }`.
- **`RemoteSubcommand::SessionStart { dir, remote_addr, api_key }`** — port `oldsrc/commands/remote.rs::run_session_start`:
  - Resolve address + api key (same as Run).
  - When `dir` is `None`, prompt the user via the per-command frontend (CLI: stdin; TUI: `RemoteSavedDirPicker` per `0069-…` §7q).
  - POST `POST /v1/sessions { working_dir }`. 200 OK → session id.
  - When the server confirms a *new* directory (response indicates `created: true`), prompt `RemoteSaveDirConfirm` (per `0069-…` §7q): on `[y]`, append to `GlobalConfig::remote.saved_dirs` and persist.
  - Return `RemoteSessionStartOutcome { session_id, working_dir, saved }`.
- **`RemoteSubcommand::SessionKill { session_id, remote_addr, api_key }`** — port `oldsrc/commands/remote.rs::run_session_kill`:
  - Resolve address + api key.
  - When `session_id` is `None`, prompt via `RemoteSessionKillPicker`.
  - DELETE `/v1/sessions/:id`. 200/204 OK or 404 (already gone) → success. Other → `CommandError::RemoteSessionKillFailed`.
  - Return `RemoteSessionKillOutcome { session_id }`.

`RemoteClient` (in `src/command/commands/remote_client.rs`) gains real impls for `send_command(req) -> Result<RemoteCommandId, ...>`, `stream_command(command_id, sink) -> Result<RemoteCommandExit, ...>` (the SSE consumer), `list_sessions(...)`, `create_session(...)`, `delete_session(...)`. HTTP timeouts per `0069-…` Edge Case "HTTP timeouts": connect=10s, read=600s for `send_command`; read disabled for `stream_command`. TLS verification mode: when the configured remote address is `127.0.0.1`/`::1` and the cert is the locally-stored self-signed cert, accept with fingerprint pinning (per `oldsrc/commands/remote.rs::tls_verifier`); otherwise standard webpki verification.

### 4. Real `AuthCommand` headless-side persistence

File: `src/command/commands/auth.rs`. The interactive consent half landed in 0070; the headless-side bits land here.

Add subcommands or flags as needed (confirm against `oldsrc/commands/auth.rs`):

- `AuthSubcommand::RefreshApiKey` (or `AuthCommand` with `--refresh-key`) — call `AuthEngine::refresh_api_key()` (real impl per §5 below). Print the new key to stderr in the legacy banner format. Return `AuthOutcome { refreshed: true, fingerprint }`.
- `AuthSubcommand::Show` — print current API key fingerprint, TLS cert fingerprint, and `auto_agent_auth_accepted` value. Return `AuthOutcome` carrying these fields.

### 5. Real `AuthEngine::ensure_self_signed_tls`

File: `src/engine/auth/mod.rs:223`. Currently returns `NotImplemented` with comment "self-signed TLS material is implemented in a later WI" / "placeholder until 0070 wires the actual self-signed flow with rcgen or similar".

Replace with real `rcgen`-based self-signed cert generation:

- Cert SAN includes the supplied `bind_ip` (typically `127.0.0.1`) and `localhost`.
- Validity: 10 years (matches old-amux).
- Subject CN: `amux-headless-<short-hash-of-bind-ip>`.
- Persist to `<HOME>/.amux/headless/tls/cert.pem` + `<HOME>/.amux/headless/tls/key.pem` (mode 0600 for the key).
- Idempotent: if both files exist and the cert's SAN matches `bind_ip`, return the existing material without regenerating.
- Fingerprint stability: SHA-256 of the DER-encoded cert. Surface as `TlsMaterial::fingerprint` so the remote command can pin against it.

Add `AuthEngine::refresh_api_key()`:

- Generate 32 random bytes, hex-encode, that's the plaintext key.
- SHA-256 hash it; persist the hash to `<HOME>/.amux/headless/api-key.hash`.
- Return `RefreshedApiKey { plaintext, hash, fingerprint: short_hex(hash[..8]) }`.

Both helpers move into `src/data/fs/headless_paths.rs` (path resolution) + `src/engine/auth/mod.rs` (cryptographic logic).

### 6. Test layout and philosophy

Same philosophy as prior layer-3 work items: **only Layer 3 unit tests + Layer 1 colocated unit tests for the new auth-engine helpers** plus **the route-parity assertion guard** (per `0069-…` §"Test Considerations"). The full parity test suite, real-loopback HTTP tests, and real-rustls cert tests are 0073's responsibility. **Do not create files under `tests/` in this work item.**

Notable additions:

- `src/engine/auth/mod.rs` — `ensure_self_signed_tls` happy path (cert + key written, fingerprint stable), idempotency (second call returns same cert), `refresh_api_key` (hash file written, plaintext returned).
- `src/frontend/headless/routes.rs` — route-parity assertion: `const EXPECTED_ROUTES: &[(&str, &str)]` table copied verbatim from `oldsrc/commands/headless/server.rs::build_router`, asserted against the new `build_router` registrations.
- `src/frontend/headless/command_frontend.rs` — `parse_command_path` data-table test covering every catalogue command + nested subcommand.
- `src/frontend/headless/auth.rs` — token mode (good/bad), disabled mode (`X-Amux-Auth: disabled` header emitted), TLS-required mode (rejects non-loopback bind without TLS).
- `src/frontend/headless/container_log.rs` — SSE wire format snapshot against frozen fixture (line-per-`data:`, `[amux:done]` sentinel).
- `src/command/commands/headless.rs` — `Start` honors flags correctly (port, background, refresh-key short-circuit, dangerously-skip-auth), `Kill` removes PID file, `Status` HTTP-probes correctly.
- `src/command/commands/remote.rs` — address resolution precedence, API-key resolution precedence (with the canonicalized-default-addr edge case), session picker prompt path, `--follow` SSE consumer, HTTP timeout configuration.

### 7. Manual sign-off checklist (gating 0073)

The PR description MUST include:

- A confirmation that `amux headless start` was run on a real machine, the server bound, every documented endpoint received a real `curl` invocation (including `--refresh-key` mode and `--background` mode), and responses were wire-compatible with pre-refactor.
- A confirmation that `amux remote run -- exec prompt "hi" --yolo` was run against a real headless server and the trailing args reached the remote without "unknown flag" errors.
- A confirmation that TLS material was generated, the cert SAN was correct, and a `curl --cacert <cert>` round-trip succeeded.
- A confirmation that `amux auth --refresh-key` printed the legacy banner exactly.
- A table of every documented headless endpoint marked PASS / MINOR-DRIFT (one-sentence justification) / REGRESSION (block).
- A confirmation that `oldsrc/` was NOT touched (other than possibly `oldsrc/README.md`).

A REGRESSION blocks the PR.

## What must NOT happen in this work item

- No business logic in `src/frontend/headless/`. If a frontend needs to make a decision that affects behavior, the missing surface is in Layer 2.
- No deletion of `oldsrc/`. That is `0073-…`.
- **No changes to the headless HTTP API surface.** No route paths, no HTTP methods, no request body fields, no response body fields.
- No edits inside `oldsrc/` other than possibly the `oldsrc/README.md` note.
- No new commands, no new flags, no new user-visible behavior. This work item closes the headless gap; it does not add to the surface.
- No tests under `tests/`. 0073 owns that tree.
- No CLI or TUI changes — those landed in 0070 / 0071. If a regression is discovered, fix it as a one-line correction with a test, but DO NOT bundle a TUI feature here.
- No Layer 1 changes outside of `AuthEngine` — every gap discovered is logged in `aspec/review-notes/0072-followups.md` for 0073, unless the gap blocks headless parity.

## Edge Case Considerations

- **PID file race on start** — two simultaneous `amux headless start` invocations: the second sees the first's PID file → if the PID is alive AND is the amux server, exit with `CommandError::HeadlessAlreadyRunning { pid }`. If the PID is dead (stale file), clean up and proceed.
- **`--background` on Windows** — Unix `fork`+`setsid` doesn't apply; use `CREATE_NEW_PROCESS_GROUP` and `CreateProcessW`. Match old-amux semantics: foreground process exits cleanly after spawning the daemon.
- **TLS cert SAN mismatch on second run** — when `bind_ip` changes between runs (e.g. user reconfigured), re-generate the cert and emit `UserMessage::warning("TLS cert regenerated for new bind IP — pinned remote clients will need to re-pin")`.
- **API key hash file missing on serve start** — when `--dangerously-skip-auth` is NOT set and the hash file doesn't exist, error with `CommandError::HeadlessAuthMissing` and a hint to run `amux auth --refresh-key`.
- **SSE backpressure** — clients that read slowly: write to the SSE channel with a bounded queue (size 256); on overflow, drop the oldest and emit `amux-message: "warning: stream backpressure — some output dropped"`. Match old-amux semantics if it had one; else ASK THE DEVELOPER.
- **WebSocket support** — `oldsrc/commands/headless/server.rs` has WebSocket handlers for some endpoints (per `0069-…` Test row 60). Confirm against the old code which routes use WS vs SSE; preserve verbatim.
- **HTTP timeouts on remote run** — connect=10s, read=600s for non-follow; follow disables read timeout (or sets to 24h). Match `oldsrc/commands/remote.rs::DEFAULT_TIMEOUTS`.
- **`--api-key` precedence with default-addr canonicalization** — `https://example.com:443` and `https://example.com/` canonicalize to the same address. Per `0069-…`; preserve.
- **Detached HEAD on remote session start** — when the remote machine's working dir is on a detached HEAD, the server emits `UserMessage::warning("detached HEAD — proceeding")` and continues. Preserve.
- **Long-running command with --follow disconnect** — when the remote client disconnects mid-stream, the command continues running on the server (it's already executing). The next `amux remote run -- get :id` (if such a command exists) re-attaches. Confirm against old behavior.
- **`auto_agent_auth_accepted` first-run consent** — None → prompt → persist; Some(true) → silent inject; Some(false) → no inject. Per `0069-…` §7h; preserve.

## Test Considerations

### Test philosophy

Layer 3 headless unit tests + Layer 1 auth-engine unit tests + the route-parity assertion guard. **Do NOT create files under `tests/`.** That tree is rebuilt from scratch in 0073.

### Build & CI

- `cargo build --release` produces a single statically-linked `amux`.
- `cargo test` passes including the new colocated tests added by this work item.
- `cargo clippy --all-targets -- -D warnings` passes.
- `make all`, `make install`, `make test` work.

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `0069-…` §3, §7u for headless specifics.
- Follow `0067-…` §9a for `AuthEngine` parity addenda.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is `0073-…`.
- Do not introduce business logic in `src/frontend/headless/`.
- Do not introduce upward calls — use traits.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, MUST include the headless parity smoke-test checklist, and MUST list every developer-clarification question raised.
- After this work item lands, the next agent picks up `0073-grand-architecture-finalize-and-remove-oldsrc.md`.
