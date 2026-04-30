# Work Item: Feature

Title: Headless Mode Part 5
Issue: issuelink

## Summary:
- Add `GET /v1/workflows/:command_id` endpoint to the headless server that returns the full workflow state for a command, persisted and updated live in `~/.amux/headless/sessions/<session-id>/commands/<command-id>/workflow.state.json` using the identical data model as local workflow state
- Add remote-bound TUI tabs: when `~/.amux/config.json` includes `remote.defaultAddr`, the new-tab dialog offers an async-fetched list of open remote sessions to bind to; tabs bound to a remote session execute all commands via the headless API instead of locally, display with purple color and the remote hostname as the tab label, and tail all output with `--follow` semantics
- When a remote-bound tab executes any command, poll `GET /v1/workflows/:command_id` every 5 seconds starting 5 seconds after the command starts; if a workflow is found, immediately render the workflow state strip and continue polling for live updates, identical to the local workflow strip experience


## User Stories

### User Story 1:
As a: developer or CI operator using the headless API

I want to: fetch the current workflow state for any running or completed command via `GET /v1/workflows/:command_id`

So I can: monitor multi-step agent workflows running on a remote headless instance, programmatically check which steps are complete or paused, and integrate workflow progress into dashboards or orchestration scripts without polling stdout

### User Story 2:
As a: developer working in the TUI

I want to: open a new tab that is permanently bound to a remote headless session, so that every command I type in that tab is sent to the remote host via the headless API

So I can: drive a remote amux server interactively from my local TUI — seeing live output and workflow progress — without having to type `remote run` before every command or manually manage session IDs

### User Story 3:
As a: developer using a remote-bound TUI tab

I want to: see the workflow state strip appear automatically when a workflow starts on the remote host after I run a command, with parallel steps, paused states, and completion rendered exactly as they appear for local workflows

So I can: monitor complex multi-step agent workflows executing on a remote machine with the same rich visual experience I have locally, without any extra commands or configuration


## Implementation Details:

### 1. Workflow state persistence in headless mode

#### 1a. Storage layout extension

Extend the existing per-command directory in `~/.amux/headless/` with a workflow state file:

```
~/.amux/headless/sessions/<session-uuid>/commands/<command-uuid>/
  stdout.log        # (existing)
  stderr.log        # (existing) — or output.log per WI 0057
  metadata.json     # (existing)
  workflow.state.json  # NEW: workflow state, written/updated as the workflow progresses
```

#### 1b. Workflow state writing during command execution (`src/commands/headless/server.rs` or `src/commands/headless/mod.rs`)

The existing `execute_command` Tokio task spawns subcommands. When the subcommand is `exec workflow` or `implement` (or any command that can produce workflow state), the task must write the workflow state file and keep it updated:

- After the subcommand starts, poll for a workflow state file in the session workdir's standard location (`~/.amux/<workflow-name>-<work-item>.state.json` or the path used by `workflow::state_path()`). Once found, copy it to the command's `workflow.state.json` on each update.
- Alternatively (preferred): inject a callback / channel into the workflow execution path so that each time `WorkflowState` is written to disk normally, it is also written to the command's `workflow.state.json`. Follow the same mechanism already used by `workflow::save_state()` in `src/workflow/mod.rs`.
- The state file must be written atomically (write to a temp file, then rename) to prevent clients from reading a partial update.
- The file must use the identical `WorkflowState` JSON format produced by `serde_json::to_string_pretty(&state)` — no new format.
- Continue writing updates until the command finishes (status `done` or `error`).

#### 1c. New API endpoint (`src/commands/headless/server.rs`)

Add a new route:

```
GET /v1/workflows/:command_id
```

Handler logic (`handle_get_workflow`):
1. Look up the command by `command_id` in the DB. Return HTTP 404 with `{"error": "command not found"}` if absent.
2. Resolve the path to `~/.amux/headless/sessions/<session-id>/commands/<command-id>/workflow.state.json`.
3. If the file does not exist, return HTTP 404 with `{"error": "no workflow for this command"}`.
4. Read the file contents and deserialize as `WorkflowState`.
5. Return HTTP 200 with the full `WorkflowState` as a JSON body.
6. Callers should not infer completion from HTTP status alone; include the `WorkflowState.status` field (or equivalent) in the body so clients can distinguish `running`, `paused`, `complete`, `error`.

Authentication: the endpoint is protected by the same auth middleware as all other endpoints (no special casing).

Add the route to the router in `build_router`:

```rust
.route("/v1/workflows/:command_id", get(handle_get_workflow))
```

#### 1d. `WorkflowState` accessibility

`WorkflowState` (defined in `src/workflow/mod.rs`) must be `pub` and its fields accessible to `server.rs`. If any field or method is currently crate-private, widen visibility to `pub` to allow serialization in the handler without duplicating the type.

---

### 2. Remote-bound TUI tabs

#### 2a. `TabState` additions (`src/tui/state.rs`)

Add new fields to `TabState` to represent a permanently remote-bound tab:

```rust
/// If set, this tab is bound to a remote headless session for its lifetime.
/// All commands are sent to this host/session via the headless API.
pub remote_binding: Option<RemoteTabBinding>,
```

```rust
/// Permanent binding of a TUI tab to a remote headless session.
#[derive(Debug, Clone)]
pub struct RemoteTabBinding {
    /// Full URL of the remote headless host (e.g. "http://1.2.3.4:9876").
    pub remote_addr: String,
    /// Session ID on the remote host.
    pub session_id: String,
    /// Resolved API key (if any) for authenticating with the remote host.
    pub api_key: Option<String>,
    /// Hostname portion extracted from `remote_addr` for display in the tab bar.
    pub display_host: String,
}
```

`display_host` is derived once at binding time by parsing the URL and extracting `host:port` (e.g. `"1.2.3.4:9876"`).

#### 2b. New-tab dialog modal changes (`src/tui/render.rs`, `src/tui/state.rs`, `src/tui/input.rs`)

**Condition:** only when `remote.defaultAddr` is set in `~/.amux/config.json`.

**Dialog state extension:** add a new variant to `Dialog` (or extend the existing new-tab modal state):

```rust
Dialog::NewTab {
    workdir_input: String,
    /// None = not yet fetched; Some(Ok(sessions)) = fetched; Some(Err(msg)) = fetch failed.
    remote_sessions: Option<Result<Vec<RemoteSessionEntry>, String>>,
    /// Index of the currently selected item in the remote sessions list (or "create new").
    remote_selected_idx: Option<usize>,
    /// Whether focus is in the workdir field (true) or the remote sessions list (false).
    focus_workdir: bool,
}
```

**Async session fetch:** when the new-tab modal opens and `remote.defaultAddr` is configured:
- Immediately begin a background Tokio task that calls `remote::fetch_sessions(addr, api_key)` (the `?status=active` filtered variant).
- When the task completes, post a TUI event (`AppEvent::RemoteSessionsFetched(Result<Vec<RemoteSessionEntry>, String>)`) to re-render the modal with the results.
- The modal renders without blocking: while the fetch is in-flight, show a single-line placeholder below the workdir field: `"  Loading remote sessions…"`.
- Do not block modal rendering or opening on the fetch.

**Modal layout (when `remote.defaultAddr` is set):**

```
┌──── New Tab ─────────────────────────────────────────────┐
│  Working directory:                                       │
│  [ /workspace/myproject                               ]   │
│                                                           │
│  ─── Remote sessions (1.2.3.4:9876) ───────────────────  │
│    abc123  /workspace/proj-a                              │
│  > def456  /workspace/proj-b          ← selected         │
│    + Create new remote session                            │
│                                                           │
│  [Enter] confirm  [Esc] cancel  [↓] move to remote list  │
└───────────────────────────────────────────────────────────┘
```

- While fetch is in progress: show `"  Loading remote sessions…"` in place of the list.
- If fetch failed: show `"  ⚠ Could not reach <host>: <short error>"` — not fatal, user can still open a local tab by pressing Enter in the workdir field.
- If fetch succeeded with no open sessions: show `"    + Create new remote session"` as the only list entry.
- Closed sessions are not shown (the `fetch_sessions` call uses `?status=active`).

**Navigation:**
- While focus is on the workdir field, `↓` moves focus to the remote sessions list (if it has been populated with at least one entry or the "create new" option).
- Within the remote sessions list, `↑`/`↓` navigate; `Enter` confirms; `Esc` cancels the entire modal.
- While focus is on the remote sessions list, `↑` at the top row returns focus to the workdir field.
- `Enter` on the workdir field (focus on workdir): open a local tab with that workdir as today (unchanged behavior).
- `Enter` on a session entry: bind the new tab to that remote session.
- `Enter` on `"+ Create new remote session"`: transition to the create-session sub-modal (section 2c).

#### 2c. Create-new-remote-session sub-modal

When the user selects `"+ Create new remote session"`, replace the new-tab modal with a session-creation modal:

```
┌──── New Remote Session ──────────────────────────────────┐
│  Remote working directory:                                │
│  [ /workspace/                                        ]   │
│                                                           │
│  Saved directories:                                       │
│    /workspace/proj-a                                      │
│  > /workspace/proj-b          ← selected                 │
│                                                           │
│  [Enter] confirm  [Esc] back  [↑↓] navigate saved dirs   │
└───────────────────────────────────────────────────────────┘
```

- Lists entries from `remote.savedDirs` (if any) using the same arrow-key navigation.
- A text field at the top allows typing a new path (selecting a saved dir populates the text field).
- `Enter` calls `remote::run_remote_session_start(remote_addr, dir)`, then if successful, creates the new tab bound to the new session.
- `Esc` returns to the new-tab modal.

#### 2d. Tab appearance for remote-bound tabs (`src/tui/render.rs`)

In `draw_tabs`:
- Tab bar color: **purple** (Ratatui `Color::Magenta` or a custom RGB if the theme supports it) for remote-bound tabs.
- Tab title label: `display_host` from `RemoteTabBinding` instead of the workdir short name.
- Inner tab label (status line / subtitle): whatever command is currently running on the remote session, exactly as local tabs show the current command. Show `"(ready)"` when no command is running (since `ready` runs automatically on creation — see 2e).

#### 2e. Command execution in remote-bound tabs (`src/tui/mod.rs`)

In `execute_command`, after parsing the command tokens, check whether the active tab has a `remote_binding`:

```rust
if let Some(ref binding) = app.active_tab().remote_binding {
    // Instead of local execution, forward to the remote host.
    launch_remote_bound_command(app, binding.clone(), raw_command_str).await;
    return;
}
```

`launch_remote_bound_command`:
1. Calls `remote::run_remote_run(remote_addr, session_id, command_tokens, follow=true, api_key, output_sink)` — equivalent to `remote run <command> --follow` but using the tab's binding fields directly, with no need for `--session`, `--remote-addr`, or `--api-key` flags.
2. All output (stdout, stderr, connection errors, auth errors) goes to the tab's execution window output channel — same sink as local commands.
3. The tab transitions through the same `ExecutionPhase` states as a local command.

**Automatic `ready` on tab creation:** when a remote-bound tab is first created, immediately enqueue a `ready` command via `launch_remote_bound_command` (same as local tabs call `ready` automatically). The `ready` output appears in the execution window.

#### 2f. New AppEvent variant (`src/tui/mod.rs` or `src/tui/events.rs`)

```rust
AppEvent::RemoteSessionsFetched(Result<Vec<RemoteSessionEntry>, String>)
```

Handled in the main event loop: set `Dialog::NewTab.remote_sessions = Some(result)` and trigger a re-render.

---

### 3. Workflow state strip for remote-bound tabs

#### 3a. Post-command workflow poll trigger

In `launch_remote_bound_command`, after spawning the command execution task, also spawn a separate Tokio task:

```rust
tokio::time::sleep(Duration::from_secs(5)).await;
// After 5s, check if a workflow was spawned for this command.
let result = remote::fetch_workflow_state(&binding.remote_addr, &command_id, &binding.api_key).await;
match result {
    Ok(Some(state)) => {
        // Workflow found — post event to start rendering and begin polling.
        tx.send(AppEvent::RemoteWorkflowStateUpdated { tab_id, command_id, state }).ok();
        // Begin 5s polling loop (see 3b).
    }
    Ok(None) => { /* 404 — no workflow — do nothing, no error */ }
    Err(_) => { /* connection/auth error — do nothing, no error */ }
}
```

`command_id` is the UUID returned by `POST /v1/commands` when the command was submitted.

#### 3b. Polling loop for workflow state

When `AppEvent::RemoteWorkflowStateUpdated` is first received and the workflow is not yet in a terminal state (`complete` / `error`), start a polling loop:

```rust
loop {
    tokio::time::sleep(Duration::from_secs(5)).await;
    match remote::fetch_workflow_state(&addr, &command_id, &api_key).await {
        Ok(Some(state)) => {
            tx.send(AppEvent::RemoteWorkflowStateUpdated { tab_id, command_id: cmd_id.clone(), state: state.clone() }).ok();
            if state.is_terminal() { break; }
        }
        Ok(None) => break, // workflow was removed — stop polling
        Err(_) => { /* transient error — keep polling */ }
    }
}
```

The polling task is associated with the tab via `tab_id`. If the tab is closed, the polling task should be cancelled (use a `CancellationToken` or check the tab's existence before each send).

#### 3c. New AppEvent variant

```rust
AppEvent::RemoteWorkflowStateUpdated {
    tab_id: usize,
    command_id: String,
    state: WorkflowState,
}
```

Handled in the main event loop: update the active tab's `workflow_state` field with the received `WorkflowState`. Trigger a re-render.

#### 3d. Rendering the workflow strip for remote-bound tabs

In `src/tui/render.rs`, the workflow state strip rendering is already driven by `TabState.workflow_state` (or the equivalent field used by local workflows). Remote-bound tabs populate the same field with the polled `WorkflowState`, so the existing strip renderer handles them with no changes.

Ensure the following edge cases are handled in the existing renderer (confirm and fix if not):
- **Paused workflow:** render with a visual pause indicator (e.g. `⏸` or `PAUSED` label) on the paused step.
- **Completed workflow:** render all steps with completion markers; stop polling.
- **Parallel steps:** render stacked as the existing local workflow strip does — no divergence from local behavior.
- **Error state:** show the failed step highlighted; stop polling.

#### 3e. `fetch_workflow_state` function (`src/commands/remote.rs`)

Add a new public function:

```rust
/// Fetch the workflow state for a command from the remote headless server.
/// Returns `Ok(None)` on HTTP 404 (no workflow for this command).
/// Returns `Ok(Some(state))` on HTTP 200.
/// Returns `Err` on network/auth errors or unexpected HTTP status.
pub async fn fetch_workflow_state(
    remote_addr: &str,
    command_id: &str,
    api_key: Option<&str>,
) -> Result<Option<WorkflowState>>;
```

Implementation:
1. Build a GET request to `{remote_addr}/v1/workflows/{command_id}` with auth header if `api_key` is set.
2. On HTTP 404: return `Ok(None)`.
3. On HTTP 200: deserialize body as `WorkflowState`, return `Ok(Some(...))`.
4. On other status or network error: return `Err(...)`.

---

### 4. Spec and parity additions

#### 4a. `src/commands/spec.rs`

The `exec workflow` and `implement` commands already exist in `ALL_COMMANDS`. No new spec entries are needed for the API endpoint itself. Document `GET /v1/workflows/:command_id` in the headless server's internal route table comment.

#### 4b. `src/commands/parity.rs`

No new `CommandId` variants are required — the remote-bound tab feature is a TUI UX enhancement that reuses existing `remote run` dispatch logic, not a new command.

#### 4c. `src/commands/config.rs`

No new config keys. `remote.defaultAddr` (from WI 0059) and `remote.defaultAPIKey` (from WI 0060) are the only config fields needed.


## Edge Case Considerations:

### Workflow state persistence
- **Command is not a workflow command:** `workflow.state.json` simply never gets created. `GET /v1/workflows/:command_id` returns HTTP 404 with `{"error": "no workflow for this command"}`. Clients treat 404 as "no workflow" and do nothing.
- **Workflow paused:** the state file reflects the paused state as written by the normal `workflow::save_state()` path. The endpoint returns the current state including the paused step; clients render it with the pause indicator. Polling continues — the workflow may resume.
- **Workflow completed:** final state is written to the file on completion. The endpoint returns the complete state. Clients stop polling once the state is terminal.
- **Concurrent reads and writes to `workflow.state.json`:** use atomic write (write to `.workflow.state.json.tmp`, then `rename`) to prevent clients from reading a partial JSON document.
- **Large workflow state:** the file is written in its entirety on each update (same as local). For very large workflows, the atomic write prevents torn reads but may cause brief contention. This is acceptable given the polling interval.
- **Command not yet started (status `pending`):** `workflow.state.json` does not yet exist. Return HTTP 404. Clients treat this as "no workflow yet."

### Remote-bound tab creation
- **`remote.defaultAddr` not configured:** the new-tab modal behaves exactly as before — no remote session list, no async fetch, no binding option. Zero behavior change.
- **Remote host unavailable at modal open time:** the async session fetch fails. The modal renders with `"  ⚠ Could not reach <host>: <error>"` below the workdir field. This is a non-fatal warning. The user can still open a local tab normally.
- **Remote host requires auth but no API key configured:** the fetch call will receive HTTP 401. Render the warning: `"  ⚠ Auth required for <host>. Set remote.defaultAPIKey or pass --api-key."`. The user can still open a local tab.
- **Session disappears between modal open and tab creation:** `POST /v1/commands` will return HTTP 404 for the closed session. Propagate the error to the tab's execution window output — same as any other remote command error.
- **User selects "Create new remote session" and the remote dir creation fails:** show modal with error text, do not create new tab, user presses esc to close error modal and then can try again with Ctrl-T to start tab creation over. 
- **New-tab modal opened while a previous remote session fetch is still in-flight:** cancel the previous fetch task before starting a new one (or ignore the stale result if it arrives after a new modal open).

### Remote-bound tab command execution
- **Auth error when sending a command:** the error appears in the tab's execution window. Polling does not start (no `command_id` was returned). The tab remains bound to the remote session and accepts the next command normally.
- **Remote session closed externally while tab is open:** `POST /v1/commands` returns HTTP 404. The error appears in the execution window. The tab remains bound; subsequent commands will also fail until the session is recreated. This is surfaced clearly — not silently ignored.
- **Tab closed while a remote command is in-flight:** cancel the SSE stream task and the workflow polling task associated with the tab. Do not emit further events for the closed tab.
- **Command contains `--session` or `--remote-addr` flags:** these are stripped before forwarding, since the binding already supplies the target — same logic as `extract_passthrough_command` in WI 0059 but applied at the launch layer, not the parse layer.

### Workflow strip polling for remote-bound tabs
- **HTTP 404 on the workflow endpoint (no workflow):** the poll task exits silently. No error, no warning to the user.
- **Transient network error during polling:** log at debug level and retry after the next 5-second interval. Do not display errors to the user for transient poll failures — only surface errors that originated from command dispatch.
- **Workflow polling outlives the execution window output stream:** the execution window may show the command as complete while polling continues. This is fine — the strip is driven by its own state.
- **Multiple commands dispatched in quick succession from the same remote-bound tab:** each command produces its own `command_id`. Only the most recent command's workflow poll task should be active. Cancel the previous poll task when a new command is dispatched from the same tab.
- **Parallel workflow steps:** `WorkflowState` already encodes parallel steps; the existing strip renderer handles them. No additional logic required.
- **Workflow step count changes between polls (dynamic workflow):** the renderer uses the live `WorkflowState` on each re-render; it handles structural changes naturally.


## Test Considerations:

### Unit tests — `src/commands/headless/server.rs`
- `GET /v1/workflows/:command_id` returns HTTP 404 when the command does not exist in the DB.
- `GET /v1/workflows/:command_id` returns HTTP 404 when the command exists but `workflow.state.json` is absent.
- `GET /v1/workflows/:command_id` returns HTTP 200 with the full `WorkflowState` JSON when the state file exists; assert all fields round-trip correctly.
- The endpoint is covered by the existing auth middleware test suite (a request with a missing key returns HTTP 401).

### Unit tests — `src/commands/remote.rs`
- `fetch_workflow_state` returns `Ok(None)` when the server returns HTTP 404.
- `fetch_workflow_state` returns `Ok(Some(state))` when the server returns HTTP 200 with valid JSON.
- `fetch_workflow_state` returns `Err` when the server returns HTTP 500.
- `fetch_workflow_state` attaches the `Authorization` header when `api_key` is `Some`.

### Unit tests — `src/tui/state.rs`
- `RemoteTabBinding` initializes correctly from a URL; `display_host` is extracted as `host:port`.
- `TabState` with `remote_binding = Some(...)` serializes/deserializes without loss (if TabState is persisted; otherwise, verify the fields exist and are accessible).

### Unit tests — `src/tui/input.rs` and `src/tui/mod.rs`
- When the active tab has `remote_binding = Some(...)`, `execute_command` calls `launch_remote_bound_command` instead of local dispatch.
- When the active tab has `remote_binding = None`, `execute_command` uses existing local dispatch (no regression).
- `AppEvent::RemoteSessionsFetched(Ok(sessions))` updates `Dialog::NewTab.remote_sessions` to `Some(Ok(sessions))`.
- `AppEvent::RemoteSessionsFetched(Err(msg))` updates `Dialog::NewTab.remote_sessions` to `Some(Err(msg))`.
- `AppEvent::RemoteWorkflowStateUpdated` updates the correct tab's workflow state field.

### Unit tests — `src/tui/render.rs`
- New-tab modal renders `"Loading remote sessions…"` when `remote_sessions = None` and `remote.defaultAddr` is set.
- New-tab modal renders the session list when `remote_sessions = Some(Ok([...]))`.
- New-tab modal renders the warning message when `remote_sessions = Some(Err(...))`.
- New-tab modal renders `"+ Create new remote session"` as the last list item.
- Remote-bound tab renders with `Color::Magenta` in the tab bar.
- Remote-bound tab displays `display_host` as the tab title label.
- Workflow state strip renders paused, running, completed, and error states correctly for remote-sourced `WorkflowState` (same rendering path as local).

### Integration tests
- Start a headless server on a random port; run `exec workflow` via `POST /v1/commands`; poll `GET /v1/workflows/:command_id` until the state is terminal; assert all steps are present and the final status matches the expected outcome.
- Start a headless server; run a non-workflow command (`exec prompt`); assert `GET /v1/workflows/:command_id` returns HTTP 404.
- Workflow state file written atomically: assert no partial JSON is ever readable during concurrent write+read in a tight loop test.
- `fetch_workflow_state` integration: spin up a test HTTP server that returns a known `WorkflowState` JSON; call `fetch_workflow_state`; assert the deserialized result matches.
- Remote-bound tab workflow polling: mock the workflow endpoint to return a workflow in `running` state, then `complete`; verify the TUI state transitions from `running` → `complete` and polling stops.

### End-to-end tests
- Open the TUI against a headless server with `remote.defaultAddr` set; open a new tab; assert the session list appears and is populated; select a session; assert the tab bar shows purple and `display_host`; type a command; assert it is forwarded via the API and output appears in the execution window.
- Run `exec workflow` from a remote-bound TUI tab; wait 5 seconds; assert the workflow strip appears and updates every 5 seconds until completion.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `WorkflowState` in `src/workflow/mod.rs` is the canonical type for all workflow state, local and remote. Use it directly in `handle_get_workflow` and `fetch_workflow_state`; do not create a parallel DTO type.
- `workflow::save_state()` (or equivalent in `src/workflow/mod.rs`) is the write path for local workflow state. Extend or wrap it to also write to the headless per-command directory when running under the headless server. A clean approach is to pass an optional `extra_state_path: Option<PathBuf>` to the save function, populated by the headless `execute_command` task.
- The new `GET /v1/workflows/:command_id` route is added in `build_router` in `src/commands/headless/server.rs` alongside all existing routes. The handler uses the same `Arc<AppState>` pattern as all other handlers.
- `RemoteTabBinding` and the new `remote_binding` field on `TabState` live in `src/tui/state.rs`. Follow the existing pattern for optional tab fields (e.g. `last_remote_session_id` from WI 0059).
- `AppEvent::RemoteSessionsFetched` and `AppEvent::RemoteWorkflowStateUpdated` are added to the event enum in `src/tui/mod.rs` or a dedicated `src/tui/events.rs`. Handle them in the main event loop's exhaustive match — do not use a wildcard arm.
- `fetch_workflow_state` is added to `src/commands/remote.rs` alongside the existing `fetch_sessions`, `stream_command_logs`, etc. It uses the same `make_client()` / `build_request()` helpers with the auth header pattern established in WI 0060.
- The remote-bound tab's automatic `ready` command on creation follows the same pattern as the existing local auto-ready logic in `src/tui/mod.rs` (wherever new tabs trigger `ready`). Call `launch_remote_bound_command(app, binding, "ready")` at tab creation time.
- The workflow poll task uses `tokio::time::sleep` in a loop. Use a `tokio::sync::CancellationToken` (add `tokio-util` if not already present, or use `tokio::select!` with a channel) to cancel the polling task when the tab is closed or a new command is dispatched.
- The new-tab modal async session fetch uses the same background task + `AppEvent` channel pattern already established in the codebase for other async TUI operations (e.g. the session picker fetch in WI 0059).
- Add `"remote"` is already in `KNOWN_SUBCOMMANDS` (WI 0059); no changes needed there. The `exec workflow` / `implement` dispatch path already handles workflow state; the headless extension is purely additive.
- Do not add new Cargo dependencies for this work item. All required types (`WorkflowState`), HTTP client (`reqwest`), and async primitives (`tokio`) are already present.
