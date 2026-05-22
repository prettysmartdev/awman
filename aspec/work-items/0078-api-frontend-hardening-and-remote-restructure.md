# Work Item: Task

Title: The Great Refocusing — Part 2: API Frontend Hardening & Remote Command Restructure
Issue: issuelink

## Summary

This work item covers four tightly related changes that together define the boundary of what the API frontend accepts and how the remote command is structured:

1. **API frontend scope restriction**: The API frontend (formerly "headless") will ONLY accept `exec workflow` and `exec prompt` commands. Any other command attempted via the API returns HTTP 400 Bad Request. This is enforced at the Layer 2 `CommandCatalogue` level — not ad-hoc in route handlers.

2. **Remote command restructure**: The `awman remote` command, which previously accepted an arbitrary command string as an argument, is restructured into concrete subcommands: `awman remote session start`, `awman remote session kill`, `awman remote exec workflow`, and `awman remote exec prompt`. No other remote subcommands exist. `awman remote session start` accepts `--type local --workdir <path>` or `--type remote --repo-url <url> --branch <branch>` (see WI 0079 for session type details).

3. **Always-yolo enforcement**: The API server always injects `--yolo` and `--non-interactive` for all `exec workflow` and `exec prompt` requests. This is enforced at the Layer 3 API frontend level, by passing these flags unconditionally when constructing the `DispatchFrontend` for API-originated exec requests. Clients cannot override this.

4. **Auto-ready on session creation**: Because `ready` is not available as an API command, the API server automatically runs `ReadyCommand` as part of session creation. This ensures container images are built and local agents are configured before any workflow job is submitted to the session.

Before implementing, read and internalize `aspec/architecture/2026-grand-architecture.md` in full. Every change must respect the four-layer boundary constraints.

## User Stories

### User Story 1:
As a: developer integrating the awman API

I want to:
receive a clear HTTP 400 Bad Request (with a descriptive error body) if I attempt to call any command other than `exec workflow` or `exec prompt` through the API

So I can:
understand immediately that those operations are not available via the API, without ambiguity or silent failure

### User Story 2:
As a: user of the remote command

I want to:
use clear, discoverable subcommands (`awman remote session start`, `awman remote exec workflow`, etc.) instead of passing raw command strings as arguments

So I can:
get proper `--help` output, flag validation, and shell completion for each remote operation without having to know the internal command string format

### User Story 3:
As a: workflow author submitting jobs via the API

I want to:
have yolo and non-interactive mode enforced server-side on all exec requests

So I can:
rely on workflows running unattended without needing to pass those flags in every API request, and without any risk of a workflow blocking waiting for interactive input

### User Story 4:
As a: platform operator creating an API session

I want to:
have awman automatically run `ready` when I create a session, without having to issue a separate request

So I can:
immediately submit exec jobs after session creation, confident that container images are built and agents are ready, even though `ready` is not available as a standalone API command


## Implementation Details

### Layer 2: Command (`src/command/`)

#### API Visibility in the CommandCatalogue
- Add a new `FrontendVisibility` variant: `ExcludeFromApi` (or more precisely, extend the existing enum so that commands can declare themselves as `ApiAllowed` vs the default which is not allowed via API).
- The preferred approach: `FrontendVisibility` gains a boolean flag `api_allowed: bool` on each command's `CommandSpec`. Only `exec workflow` and `exec prompt` have `api_allowed: true`.
- `CommandCatalogue` exposes a method `api_allowed_commands() -> &[CommandSpec]` which returns only the API-permitted subset.
- The `Dispatch` layer uses this catalogue method to validate incoming API requests before routing them to a command — this validation lives in `Dispatch`, NOT in the Layer 3 route handlers.
- `Dispatch` exposes a method such as `validate_frontend_command(frontend: FrontendKind, command: &str, subcommand: &str) -> Result<(), DispatchError>` where `FrontendKind` is a new Layer 2 enum with variants `Cli`, `Tui`, `Api`. This is called by the API frontend before executing any command.

#### Remote Command Restructure
- Delete the existing `RemoteCommand` implementation that accepts an arbitrary command string argument.
- Replace with a new `RemoteCommand` that defines concrete subcommands via the `CommandCatalogue`:
  - `remote session start` — starts a remote awman API session (wraps the `POST /sessions` HTTP call)
  - `remote session kill` — kills a remote awman API session (wraps `DELETE /sessions/{id}`)
  - `remote exec workflow` — submits an `exec workflow` job to a remote awman API server
  - `remote exec prompt` — submits an `exec prompt` job to a remote awman API server
- Each subcommand has its own flag set registered in `CommandCatalogue`:
  - `remote session start` flags: `--type <local|remote>` (required), `--workdir <path>` (required when `--type local`), `--repo-url <url>` (required when `--type remote`), `--branch <name>` (optional, used when `--type remote`; defaults to the remote's default branch if omitted)
  - `remote exec workflow` flags: mirrors the local `exec workflow` flag set, minus `--workdir` (which is set server-side by the session); derive this list programmatically from the core `exec workflow` CommandSpec; additionally adds `--follow` (bool, default false in CLI, always true in TUI — see "Remote Follow Mode" below)
  - `remote exec prompt` flags: similarly derived from local `exec prompt`, plus `--follow`
  - `remote session kill`: similarly derived

#### Remote Follow Mode
`--follow` on `remote exec workflow` and `remote exec prompt` causes the client to wait for the job to complete and stream progress back to the user. The behavior differs between CLI and TUI frontends.

**In CLI** (`--follow` is a boolean flag, default `false`):
- After the job is submitted and the `job_id` is returned, if `--follow` is `true`:
  - Enter a poll loop (every 500ms): call `RemoteClient::get_job(session_id, job_id)` to check overall job status
  - For `exec workflow` jobs, additionally call `RemoteClient::get_workflow_state(session_id, job_id)` on each poll
  - On each poll that returns new or changed workflow state, print step status changes to stdout in a compact format, e.g.:
    ```
    [setup]    clone_repo   → running
    [setup]    clone_repo   → done
    [step 1]   implement    → running
    [step 1]   implement    → done ✓
    [teardown] create_pr    → running
    ```
  - Continue polling until job status is `completed` or `failed`
  - Print the final job outcome and exit with the job's exit code
- If `--follow` is `false` (default): print the `job_id` and return immediately; the user polls manually

**In TUI** (`--follow` is always `true` and not configurable):
- After job submission, the TUI immediately begins background polling (see WI 0079 — "TUI Workflow Strip for Remote Sessions")
- The workflow strip renders step progress using the same `workflow_view.rs` rendering as local workflows
- The tab remains active and responsive while polling runs in the background

**`RemoteClient` new methods** (Layer 2, `remote_client.rs`):
- `get_job(session_id: &SessionId, job_id: &JobId) -> Result<JobRecord>` — `GET /sessions/{id}/jobs/{job_id}`
- `get_workflow_state(session_id: &SessionId, job_id: &JobId) -> Result<Option<WorkflowState>>` — `GET /sessions/{id}/jobs/{job_id}/workflow`; returns `None` on HTTP 404 (job pending or prompt job)
- `RemoteCommand` is instantiated by `Dispatch` like all other commands; it receives a `RemoteCommandFrontend` trait that provides connection details (host, port, API key) from the frontend
- The `RemoteClient` (HTTP client in `remote_client.rs`) is updated to only expose methods matching these four concrete operations; remove any general "run arbitrary command" method

#### Auto-Ready on API Session Creation
- `ReadyCommand` is a Layer 2 command. When `CreateSessionCommand::run(...)` (see WI 0079) completes session setup, it calls `ReadyCommand::run_for_session(session, engines, frontend)` before returning.
- `ReadyCommand::run_for_session` performs all the same work as `ready` invoked from the CLI: verifies the base container image is built (building if not), verifies agent configurations, etc.
- If `ReadyCommand` fails (e.g. Docker daemon unavailable, image build fails), `CreateSessionCommand` must:
  - For `remote` sessions: clean up the cloned repository directory before returning the error
  - For `local` sessions: return the error without any filesystem cleanup (the workdir belongs to the user)
  - In both cases, do NOT persist a session record — the session creation is considered failed
- The Layer 3 handler returns HTTP 503 (Service Unavailable) if `ReadyCommand` fails during session creation, with a body describing which readiness check failed
- `ReadyCommand::run_for_session` must NOT require interactive user input — it must run non-interactively in all cases when called during session creation. If `ready` currently has interactive prompts (e.g. "would you like to rebuild the image?"), those must be suppressed and the non-interactive default must be applied. Add a `run_non_interactive` method or a `non_interactive: bool` parameter to `ReadyCommand` if not already present.
- `ReadyCommand` must NOT be added to the API `CommandCatalogue` as a directly-invocable command — it is never exposed as an API endpoint. Its auto-run during session creation is an internal Layer 2 concern only.

#### Always-Yolo Enforcement
- In `ExecWorkflowCommand` and `ExecPromptCommand`, add a method to the per-command frontend trait: `fn is_api_frontend(&self) -> bool`. When `true`, the command unconditionally sets `yolo = true` and `non_interactive = true` in the resolved `EffectiveConfig`, regardless of what was passed in flags.
- Alternatively (preferred for cleaner separation): the `ApiFrontend`'s implementation of `ExecWorkflowCommandFrontend` and `ExecPromptCommandFrontend` always returns `true` for `flag_bool("yolo")` and `flag_bool("non-interactive")`, and `Dispatch` passes those values when constructing the command. This means the enforcement lives in the Layer 3 API frontend's trait implementation — the command itself doesn't need to know it's being called from the API.
- There is no mechanism for API clients to pass `--yolo false` or disable non-interactive mode. The Layer 3 `ApiFrontend` ignores any such flags in the request payload and always returns `true` for these two flags.

### Layer 3: Frontend (`src/frontend/api/`)

#### Route Hardening
- The API route dispatcher calls `Dispatch::validate_frontend_command(FrontendKind::Api, cmd, subcmd)` before routing any request. If validation fails, the handler returns HTTP 400 with a JSON body: `{ "error": "command not available via API", "available": ["exec workflow", "exec prompt"] }`.
- This replaces any ad-hoc command string matching that may currently exist in route handlers.
- Route handlers themselves remain thin — they translate HTTP request bodies into `DispatchFrontend` trait implementations and call `Dispatch::run_command(...)`. No business logic lives in route handlers.

#### Remote Client Updates
- `RemoteCommand`'s HTTP client (`remote_client.rs`) is updated to only call the concrete API routes that exist on the server. The client is now a typed client with methods: `start_session(...)`, `kill_session(...)`, `exec_workflow(...)`, `exec_prompt(...)`. No generic "send command string" method.

### Layer 0: Data (`src/data/`)
- No schema changes required for this work item.
- If `RemoteCommand` persists any connection config (host, port, key fingerprint), ensure those types remain in Layer 0 and are named to reflect the concrete subcommand structure.

### Layer 1: Engine (`src/engine/`)
- No changes required. The always-yolo enforcement is purely a flag resolution concern handled in Layer 2/3.


## Edge Case Considerations

- **Existing API clients**: Any client that currently calls non-exec endpoints (e.g. `GET /sessions`, `POST /sessions`) is unaffected — those session management routes remain. Only exec-related routes that aren't `exec workflow` or `exec prompt` are rejected. Clarify in the API route table which routes are session management (always allowed) vs command execution (restricted).
- **Remote command backward compat**: The old `awman remote <command-string>` invocation will no longer work. The CLI will emit a clear error: "the `remote` command now requires a subcommand. See `awman remote --help`." Do not silently try to parse old invocations.
- **Flag conflict — yolo override**: If an API client passes `yolo: false` in the exec request body, the server silently overrides it to `true`. Document this behavior clearly in the API response: consider including a `"flags_applied": { "yolo": true, "non_interactive": true }` field in exec responses so clients know what was enforced.
- **Remote exec flags**: `awman remote exec workflow` must accept the same flags as local `awman exec workflow` (minus flags that make no sense remotely, like `--workdir` which is set server-side). Ensure the `CommandCatalogue` for `remote exec workflow` declares the correct flag set — do not manually duplicate the flag list; derive it programmatically from the core `exec workflow` command spec, minus remote-excluded flags.
- **Dispatch error propagation**: `Dispatch::validate_frontend_command` returning an error must propagate cleanly to the Layer 3 handler as a typed `DispatchError::NotAvailableForFrontend` variant — not a raw string. The Layer 3 handler converts this to HTTP 400.
- **Ready failure during session creation**: If `ReadyCommand` fails during session creation (e.g. Docker daemon is not running), the error message must be surfaced clearly in the HTTP 503 response body. The operator must resolve the underlying issue (e.g. start Docker) and retry `POST /sessions`. Do not silently create a session in an "unready" state and allow jobs to be queued against it.
- **Ready idempotency**: `ReadyCommand::run_for_session` may be called multiple times across the lifetime of an awman server process (once per session creation). It must be idempotent — if the base image is already built and agents are already configured, it must complete quickly without rebuilding or re-initializing.
- **`--follow` with `exec_prompt` jobs**: Prompt jobs have no workflow state file (only `exec_workflow` produces one). `RemoteClient::get_workflow_state` returns `None` for prompt jobs. In CLI `--follow` mode for `exec prompt`, poll only job status (not workflow state) and print the final output when the job completes.
- **`--follow` poll interruption**: If the user hits Ctrl-C during CLI `--follow` polling, terminate cleanly — print "Follow interrupted. Job is still running server-side. Check status with: awman remote exec workflow --session {id} --job {job_id} status." Do not kill the server-side job.
- **Polling interval vs server load**: The 500ms poll interval is intentionally conservative. Do not make it configurable in this work item — the API does not support server-sent events for the new queue system.
- **`--follow` across server restart**: If the awman API server restarts while a CLI client is in `--follow` poll mode, the poll will receive connection errors. After 3 consecutive connection failures, the CLI should exit with an error message explaining the server may have restarted.
- **`remote session start --type remote --branch` defaults**: If `--branch` is omitted when creating a remote-type session, the session uses the remote repository's default branch. `GitEngine::clone_repo` without an explicit branch should clone the default branch and `checkout_or_create_branch` should not be called in this case (the default branch is already checked out after clone).


## Test Considerations

- **API rejection tests**: For each command that is NOT `exec workflow` or `exec prompt`, send the request via the API frontend test harness and assert HTTP 400 is returned with the expected JSON error body.
- **API acceptance tests**: `exec workflow` and `exec prompt` via the API frontend return non-400 responses (even if the workflow itself fails, the routing must succeed).
- **Always-yolo test**: Submit an `exec prompt` request via the API with `yolo: false` in the payload; assert the executed command ran with yolo enabled (verify via workflow state or container invocation args).
- **Remote subcommand help test**: `awman remote --help` lists exactly the four subcommands and no others. `awman remote session --help` lists `start` and `kill`. `awman remote exec --help` lists `workflow` and `prompt`.
- **Remote session start flags test**: `awman remote session start --help` lists `--type`, `--workdir`, `--repo-url`, and `--branch`. Invoking with `--type remote` without `--repo-url` returns a validation error.
- **Remote old-style rejection test**: Invoking `awman remote chat` (old arbitrary-command style) returns a clear error message pointing users to `awman remote --help`.
- **Remote exec workflow integration test**: `awman remote exec workflow --workflow foo` sends the correct HTTP request to a mock API server and the response is correctly parsed.
- **Dispatch catalogue unit test**: `CommandCatalogue::api_allowed_commands()` returns exactly `[("exec", "workflow"), ("exec", "prompt")]` and nothing else.
- **FrontendKind validation unit test**: `Dispatch::validate_frontend_command(FrontendKind::Api, "chat", "")` returns `Err(DispatchError::NotAvailableForFrontend)`.
- **Auto-ready success test**: Create a session via `POST /sessions`; assert `ReadyCommand::run_for_session` was called and completed before the response is returned.
- **Auto-ready failure test**: Mock `ReadyCommand` to return an error; assert `POST /sessions` returns HTTP 503 with the error message, and no session record is created in `ApiDb`.
- **Ready idempotency test**: Call `ReadyCommand::run_for_session` twice on the same session; assert the second call completes without error and without triggering a full rebuild (use a mock or spy on `ContainerRuntime`).
- **`ready` not in API catalogue test**: Assert `CommandCatalogue::api_allowed_commands()` does not include `ready`.
- **`--follow` CLI poll loop test**: Submit a mock exec workflow job, enable `--follow`; assert the CLI polls `get_job` and `get_workflow_state` at least once before the job completes and prints step status lines to stdout.
- **`--follow` prompt job test**: Submit a mock exec prompt job with `--follow`; assert `get_workflow_state` is never called (only `get_job` is polled) and the CLI exits cleanly when job completes.
- **`--follow` Ctrl-C test**: Assert that the CLI exits cleanly on interrupt during polling and prints the "job still running" message.
- **`--follow` server disconnect test**: Mock 3 consecutive connection failures from `get_job`; assert the CLI exits with an appropriate error message.
- **TUI always-follow test**: In TUI mode, assert that submitting `remote exec workflow` always starts background polling regardless of any flag value.


## Codebase Integration

- Strictly follow `aspec/architecture/2026-grand-architecture.md`. The canonical command list and `api_allowed` flags live in Layer 2 (`CommandCatalogue`). Validation logic lives in `Dispatch` (Layer 2). The Layer 3 `ApiFrontend` only calls `Dispatch` to validate — it does not implement its own allowlist.
- The `FrontendKind` enum must live in Layer 2 (not Layer 3), since it is used by `Dispatch` which is a Layer 2 concern.
- The always-yolo enforcement must NOT be implemented as a special-case `if is_api` branch inside `ExecWorkflowCommand` or `ExecPromptCommand` in Layer 2. Instead, the Layer 3 `ApiFrontend`'s trait implementation returns `true` for these flags unconditionally — this keeps the command layer frontend-agnostic.
- `RemoteCommand` in Layer 2 defines the concrete subcommand structure. `RemoteClient` in Layer 2 implements the HTTP calls. Layer 3 CLI frontend just invokes `RemoteCommand` through `Dispatch` like any other command.


## Documentation

After implementation:
- `docs/08-api-mode.md` — document the restricted command set: only `exec workflow` and `exec prompt` are accepted; include the JSON error response format for rejected commands; add a section on auto-ready at session creation and what it does; note that `--yolo` and `--non-interactive` are always applied by the server
- `docs/09-remote-mode.md` — rewrite to document the four concrete `awman remote` subcommands with flag details and usage examples for both local and remote session types; remove any reference to arbitrary command passthrough
