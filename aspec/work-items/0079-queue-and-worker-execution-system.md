# Work Item: Task

Title: The Great Refocusing — Part 3: Queue-and-Worker Execution System
Issue: issuelink

## Summary

The awman API frontend is moving from a synchronous request-response model to an async queue-and-worker execution system. API clients submit "exec jobs" (for `exec workflow` or `exec prompt`) which are enqueued in a SQLite-backed job queue. Worker tasks self-assign jobs from the queue and execute them. Clients poll per-session job status asynchronously. This is single-node only.

Sessions in this model have an explicit **type** that governs how the working directory is provisioned:

- **`local`**: The session is bound to an existing directory already present on the host (e.g. a repo the operator has already cloned). The client supplies an absolute `workdir` path when creating the session. awman does not manage the directory lifecycle.
- **`remote`**: The session is bound to a remote git repository. When the session is created, `GitEngine` clones the repository into an isolated directory under `~/.awman/sessions/{session_id}/repo/`. When the session is killed, `GitEngine` deletes that directory. The client supplies a `repo_url` and optionally a `branch` when creating the session.

For `remote` sessions, because the cloned repository already occupies its own isolated directory, **no git worktree is created** when running `exec workflow` or `exec prompt`. The command layer (`ExecWorkflowCommand`) is responsible for detecting the session type and skipping worktree creation accordingly.

Both session types use the same job queue and worker system.

Before implementing, read and internalize `aspec/architecture/2026-grand-architecture.md` in full. The session types, job queue schema, and all persistence types live in Layer 0. `GitEngine` methods for clone/checkout/delete live in Layer 1. Session creation lifecycle coordination (clone on create, delete on kill, worktree suppression) lives in Layer 2. The API frontend (Layer 3) only starts workers, exposes HTTP routes, and delegates all logic to lower layers.

## User Stories

### User Story 1:
As a: API client

I want to:
submit an exec job via `POST /sessions/{id}/jobs` and immediately receive a job ID, then poll `GET /sessions/{id}/jobs/{job_id}` to check status (pending, running, completed, failed)

So I can:
submit multiple long-running workflows without blocking and check results when ready, without holding open an HTTP connection

### User Story 2:
As a: platform operator using the API to run workflows against a remote git repository

I want to:
create a `type: remote` session by supplying a `repo_url` and `branch`, and have awman clone the repo and check out (or create) the branch automatically

So I can:
run workflows against a fresh, isolated copy of a remote repository without needing to pre-clone it on the host machine

### User Story 3:
As a: platform operator

I want to:
have worker tasks self-assign jobs atomically from the SQLite queue so that if multiple worker tasks are running, no job is executed twice

So I can:
scale the number of workers within a single awman process without risking duplicate job execution


## Implementation Details

### Layer 0: Data (`src/data/`)

#### SessionType
- Add a new enum in `src/data/session.rs`:
  ```rust
  pub enum SessionType {
      Local { workdir: PathBuf },
      Remote { repo_url: String, branch: String, cloned_path: PathBuf },
  }
  ```
- `Session` gains a `session_type: SessionType` field, replacing any prior ad-hoc workdir field. `Session::working_dir()` becomes a method that returns the appropriate path: for `Local`, it returns `workdir`; for `Remote`, it returns `cloned_path`.
- `SessionType` derives `serde::Serialize` and `serde::Deserialize` — persisted as a JSON column in the `sessions` table.
- For `Remote` sessions, `cloned_path` is deterministic: `~/.awman/sessions/{session_id}/repo/`. This path is computed by Layer 0 path helpers in `api_paths.rs` — not hardcoded in Layer 2.
- Add `SessionType::is_remote(&self) -> bool` and `SessionType::cloned_path(&self) -> Option<&Path>` helpers.

#### Job Queue Schema
- New module `src/data/fs/api_job_queue.rs` (consistent with the renamed `api_db.rs` from WI 0077)
- SQLite schema additions (new tables, added to the existing API database managed by `ApiDb`):

```sql
CREATE TABLE sessions (
    session_id   TEXT PRIMARY KEY,
    session_type TEXT NOT NULL,  -- JSON-serialized SessionType
    status       TEXT NOT NULL DEFAULT 'active',  -- 'active' | 'closed'
    created_at   TEXT NOT NULL,
    closed_at    TEXT
);

CREATE TABLE jobs (
    job_id       TEXT PRIMARY KEY,       -- UUID
    session_id   TEXT NOT NULL,
    job_type     TEXT NOT NULL,          -- "exec_workflow" | "exec_prompt"
    payload      TEXT NOT NULL,          -- JSON: workflow name/path, model, agent, etc.
    status       TEXT NOT NULL           -- "pending" | "running" | "completed" | "failed"
                 DEFAULT 'pending',
    worker_id    TEXT,                   -- NULL until claimed
    created_at   TEXT NOT NULL,
    claimed_at   TEXT,
    completed_at TEXT,
    result       TEXT                    -- JSON: exit code, error message, output summary
);

CREATE INDEX idx_jobs_session ON jobs(session_id);
CREATE INDEX idx_jobs_status  ON jobs(status, created_at);
```

- New types in Layer 0:
  - `JobId` (newtype over UUID, serializable)
  - `WorkerId` (newtype over UUID, identifies a running worker task)
  - `JobType` enum: `ExecWorkflow`, `ExecPrompt`
  - `JobStatus` enum: `Pending`, `Running`, `Completed`, `Failed`
  - `JobPayload` struct: `workflow_or_prompt: String`, `agent: Option<AgentName>`, `model: Option<String>` — serialized as JSON into `payload` column
  - `JobResult` struct: `exit_code: Option<i32>`, `error: Option<String>` — serialized as JSON into `result` column
  - `JobRecord` struct: mirrors the jobs table row exactly, derives `serde::Serialize` and `serde::Deserialize`

- New `JobQueue` struct (thin wrapper over the `ApiDb` connection pool):
  - `JobQueue::enqueue(session_id, job_type, payload) -> Result<JobId>` — inserts a new job with `status = 'pending'`
  - `JobQueue::claim_next(worker_id) -> Result<Option<JobRecord>>` — atomically claims the next pending job using a SQLite transaction: `UPDATE jobs SET status='running', worker_id=?, claimed_at=? WHERE job_id = (SELECT job_id FROM jobs WHERE status='pending' ORDER BY created_at ASC LIMIT 1)`. Returns the claimed record or `None` if queue is empty.
  - `JobQueue::complete_job(job_id, result) -> Result<()>` — sets status to `completed`, writes result JSON
  - `JobQueue::fail_job(job_id, error) -> Result<()>` — sets status to `failed`, writes error to result
  - `JobQueue::list_by_session(session_id) -> Result<Vec<JobRecord>>` — returns all jobs for a session, ordered by `created_at`
  - `JobQueue::get_job(job_id) -> Result<Option<JobRecord>>`

#### Remote Session Path Helpers
- Add to `api_paths.rs`:
  - `fn remote_session_repo_path(session_id: &SessionId) -> PathBuf` → `~/.awman/sessions/{session_id}/repo/`
  - `fn remote_session_dir(session_id: &SessionId) -> PathBuf` → `~/.awman/sessions/{session_id}/`
  - `fn job_state_dir(session_id: &SessionId, job_id: &JobId) -> PathBuf` → `~/.awman/sessions/{session_id}/jobs/{job_id}/`
  - `fn job_workflow_state_path(session_id: &SessionId, job_id: &JobId) -> PathBuf` → `~/.awman/sessions/{session_id}/jobs/{job_id}/workflow_state.json`
- These are pure path functions — no I/O, no side effects. They are called by Layer 2 when constructing sessions and by Layer 1 when cleaning up.

#### WorkflowState Step Metadata (Layer 0 — coordinate with WI 0080)
WI 0080 extends `WorkflowState` with phase tracking fields. This work item additionally requires that `WorkflowState` be **self-describing** for remote rendering — i.e. it must carry enough information for a TUI or CLI client to reconstruct the full step list with dependency topology without separately fetching the `WorkflowDefinition`.

Add to `WorkflowState` in `src/data/workflow_state.rs`:
```rust
pub steps: Vec<WorkflowStepInfo>,
```
where:
```rust
pub struct WorkflowStepInfo {
    pub name: String,
    pub depends_on: Vec<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
}
```
`WorkflowEngine` populates `steps` from the `WorkflowDefinition` when the workflow is first created (during `WorkflowEngine::new`). This field does not change after initialization. It enables a polling client to render the full topological workflow strip without access to the definition file.

Also add (coordinate with WI 0080's phase step tracking):
```rust
pub setup_step_states: Vec<PhaseStepState>,
pub teardown_step_states: Vec<PhaseStepState>,
```
where:
```rust
pub struct PhaseStepState {
    pub description: String,  // human-readable (e.g. "clone_repo: https://github.com/org/repo")
    pub status: PhaseStepStatus,  // Pending | Running | Succeeded | Failed { error: String }
}
```
`WorkflowEngine::run_setup` and `run_teardown` update these vecs (via the `WorkflowFrontend` trait callbacks) after each step transitions. This makes setup/teardown step progress visible via the workflow status API endpoint.

Bump `WORKFLOW_STATE_SCHEMA_VERSION` once for both this and WI 0080's changes — coordinate to avoid double-bumping.

### Layer 1: Engine (`src/engine/`)

#### GitEngine — New Methods for Remote Session Lifecycle
- `GitEngine::clone_repo(url: &str, branch: &str, into_path: &Path) -> Result<()>` — clones the remote repo at `url` into `into_path`, checking out `branch`. If the target directory already exists and is non-empty, return `GitError::CloneTargetExists`. Creates parent directories as needed.
- `GitEngine::checkout_or_create_branch(repo_path: &Path, branch: &str) -> Result<BranchDisposition>` — inspects the repository at `repo_path` for the given `branch`:
  - If the branch exists on the remote (i.e. is in `git branch -r`): checkout with `git checkout <branch>`
  - If the branch does not exist remotely: create it locally with `git checkout -b <branch>`
  - Returns `BranchDisposition::CheckedOut` or `BranchDisposition::Created` so the caller can log the appropriate message
- `GitEngine::delete_directory(path: &Path) -> Result<()>` — removes the directory and all contents. Returns `GitError::DirectoryNotFound` if the path does not exist. This is a destructive filesystem operation — callers must only use it for awman-managed directories (remote session repos under `~/.awman/`).

  > Note: `delete_directory` uses `std::fs::remove_dir_all`. It lives in Layer 1 (`GitEngine`) rather than Layer 0 because it is explicitly a git-lifecycle operation (cleaning up a cloned repo), not generic file I/O. Layer 0 path helpers provide the path; Layer 1 performs the deletion.

- `BranchDisposition` enum: `CheckedOut`, `Created` — Layer 1 type, returned by `checkout_or_create_branch`.

#### WorkflowEngine — No New Methods
- No changes required to `WorkflowEngine` for session type handling. The worktree suppression decision is made in Layer 2 before `WorkflowEngine` is invoked.

### Layer 2: Command (`src/command/`)

#### Session Creation Command (`CreateSessionCommand`)
- New type `CreateSessionCommand` in `src/command/commands/create_session.rs`. This command is invoked by the API frontend when `POST /sessions` is received. It is NOT available from the CLI or TUI (visibility: `ApiOnly` in the `CommandCatalogue`).
- `CreateSessionCommand::run(session_type: SessionType, engines: &Engines, session_manager: &SessionManager) -> Result<Session>`:
  1. If `session_type` is `Remote`:
     a. Compute `cloned_path = api_paths::remote_session_repo_path(&new_session_id)`
     b. Call `engines.git_engine.clone_repo(&repo_url, &branch, &cloned_path)`
     c. Call `engines.git_engine.checkout_or_create_branch(&cloned_path, &branch)` — log the `BranchDisposition` result
     d. If clone or checkout fails, abort — do NOT create a session record
  2. Create a `Session` with the resolved `session_type` and `working_dir`
  3. Persist the session to `ApiDb` via `SessionManager`
  4. Trigger `ReadyCommand` (see WI 0078 — auto-ready on session creation)
  5. Return the created `Session`

#### Session Kill Command (`KillSessionCommand`)
- `KillSessionCommand::run(session_id, engines, session_manager) -> Result<()>`:
  1. Refuse if any jobs for the session are in `running` status (return `SessionError::JobsStillRunning`)
  2. If `session.session_type` is `Remote`:
     a. Call `engines.git_engine.delete_directory(&session.session_type.cloned_path())`
  3. Mark the session as `closed` in `ApiDb`
  4. Remove from in-memory `SessionManager`

#### ExecWorkflowCommand — Worktree Suppression for Remote Sessions
- In `ExecWorkflowCommand::run_with_frontend(session, ...)`, before the worktree creation step, check `session.session_type.is_remote()`. If `true`, skip worktree creation entirely. The workflow runs directly in `session.working_dir()` (which is already the isolated `cloned_path`). Log a debug note: "Skipping worktree creation for remote session — repo is already isolated."
- This check must live in `ExecWorkflowCommand` at Layer 2, not in `WorkflowEngine` at Layer 1. `WorkflowEngine` must not be aware of session types.

#### QueueWorker
- New type: `QueueWorker` in `src/command/queue_worker.rs`
- `QueueWorker` holds a `JobQueue` (Layer 0) reference, a `WorkerId`, and access to `Dispatch` and `Engines` (same bundle as other command execution paths)
- `QueueWorker::new(job_queue, worker_id, engines, session_manager) -> QueueWorker`
- `QueueWorker::run(self) -> !` — async loop: calls `job_queue.claim_next(worker_id)` in a loop. If a job is returned, execute it. If no job, sleep briefly (e.g. 250ms) and retry. This loop runs indefinitely as a `tokio::task`.
- Job execution within `QueueWorker::run`:
  1. Look up the session from `SessionManager` using `job.session_id`
  2. Compute the job state directory: `api_paths::job_state_dir(&job.session_id, &job.job_id)`. Create this directory on disk (`std::fs::create_dir_all`).
  3. Construct a per-job `WorkflowStateStore::at_path(job_state_dir)` and inject it into a **per-job copy of `Engines`** with this store replacing the default store. This ensures `WorkflowEngine` writes state to `~/.awman/sessions/{session_id}/jobs/{job_id}/workflow_state.json` for this job, not to the session's git root.
  4. Construct a `DispatchFrontend` implementation (`QueueWorkerFrontend`) that reads flags from the job payload, always returns `true` for `yolo` and `non_interactive`, and sends all output to a log sink associated with the job record
  5. Call `Dispatch::run_command(job_type, ...)` using the resolved command, `QueueWorkerFrontend`, and the per-job `Engines` copy
  6. On completion, call `job_queue.complete_job(job_id, result)` or `job_queue.fail_job(job_id, error)`
- `QueueWorkerFrontend` is a Layer 2 struct (lives in `src/command/`) that implements `DispatchFrontend`. It reads all flag values from the `JobPayload` and always enforces yolo/non-interactive.
- The number of concurrent workers is configurable via global config (`awman.workers: u8`, default 2). The `QueueWorker` tasks are spawned by Layer 3 at server startup.

#### Worker Spawn Count
- `GlobalConfig` in Layer 0 gains a `workers: Option<u8>` field (defaults to 2 if not set)
- Layer 3 reads this value at server startup and spawns that many `QueueWorker::run()` tokio tasks

### Layer 3: Frontend (`src/frontend/api/`)

#### Server Startup
- After session restore (existing logic), spawn N worker tasks: `for _ in 0..global_config.workers() { tokio::spawn(QueueWorker::new(...).run()); }`
- Workers are fire-and-forget tasks; the server does not await them.

#### New HTTP Routes
All routes follow the pattern: translate request → call Layer 2 → return response. No business logic in handlers.

- `POST /sessions` — create a session. Request body is one of:
  ```json
  { "type": "local", "workdir": "/absolute/path/to/repo" }
  ```
  or
  ```json
  { "type": "remote", "repo_url": "https://github.com/org/repo", "branch": "my-feature" }
  ```
  The handler calls `CreateSessionCommand::run(session_type, engines, session_manager)`. Response:
  ```json
  {
    "session_id": "...",
    "type": "remote",
    "workdir": "~/.awman/sessions/.../repo",
    "branch": "my-feature",
    "branch_disposition": "created"
  }
  ```
- `DELETE /sessions/{id}` — kill a session. Calls `KillSessionCommand::run(...)`. Returns HTTP 409 if jobs are still running.
- `POST /sessions/{id}/jobs` — enqueue a job. Request body: `{ "type": "exec_workflow", "workflow": "my-workflow", "agent": "claude", "model": "..." }`. Response: `{ "job_id": "...", "status": "pending" }`.
- `GET /sessions/{id}/jobs` — list all jobs for the session.
- `GET /sessions/{id}/jobs/{job_id}` — get a specific job's full record including result.
- `GET /sessions/{id}/jobs/{job_id}/workflow` — get the current `WorkflowState` for an `exec_workflow` job. The handler:
  1. Calls `api_paths::job_workflow_state_path(&session_id, &job_id)` to resolve the file path (Layer 0 path helper — no direct path construction in Layer 3)
  2. If the file does not exist (job still pending, or job type is `exec_prompt`): return HTTP 404 with `{ "error": "no workflow state for this job" }`
  3. If the file exists: read and deserialize as `WorkflowState`, return HTTP 200 with the full JSON body
  4. On deserialization failure: return HTTP 500 with an error message
  - This endpoint is polled by CLI `--follow` mode and by the TUI's remote workflow strip. It is read-only — the file is written exclusively by `WorkflowEngine` via `WorkflowStateStore`.

All route handlers call into Layer 2 types or Layer 0 path/file helpers — no direct SQLite calls in Layer 3.

#### TUI Workflow Strip for Remote Sessions
When the TUI submits a `remote exec workflow` job (via `Dispatch`), it must display the workflow strip and update it in real time from the API. Follow mode is always active in the TUI.

**RemoteWorkflowPoller** (new type in `src/frontend/tui/`, Layer 3 TUI):
```rust
struct RemoteWorkflowPoller {
    client: Arc<RemoteClient>,
    session_id: SessionId,
    job_id: JobId,
    workflow_view: Arc<Mutex<Option<WorkflowViewState>>>,
}
```
- `RemoteWorkflowPoller::start(self) -> JoinHandle<()>` — spawns a `tokio::task` that:
  1. Every 500ms: call `client.get_job(session_id, job_id)` for overall status
  2. Every 500ms (same tick): call `client.get_workflow_state(session_id, job_id)`
  3. On a new `WorkflowState` response: convert it to `WorkflowViewState` via `workflow_state_to_view_state(&state)`
  4. Lock `workflow_view`, replace the current value, release
  5. The TUI render loop picks up the updated `WorkflowViewState` and redraws the workflow strip on the next tick — identical rendering path to local workflows
  6. When job status is `completed` or `failed`: do one final state poll, update the view, then stop polling
- `RemoteWorkflowPoller` is created in the TUI command handling path immediately after a `remote exec workflow` job is submitted. The `workflow_view` Arc is the **same one already used by the tab for local workflow rendering** — no special remote strip path is needed; the strip renders identically regardless of whether data comes from local `WorkflowEngine` callbacks or remote polling.

**`workflow_state_to_view_state` conversion function** (Layer 3 TUI — may be promoted to Layer 2 if CLI reuse is needed):
- Input: `&WorkflowState` (Layer 0 type — see Layer 0 section above for `WorkflowStepInfo`, `PhaseStepState` additions)
- Output: `WorkflowViewState` (TUI type containing `Vec<WorkflowStepView>`)
- Conversion:
  1. Prepend a pseudo-step for each entry in `state.setup_step_states` (if non-empty): `WorkflowStepView { name: phase_step.description, status: mapped_from(phase_step.status), depends_on: [] }`. These represent setup container exec steps and appear at the top of the strip labeled with their description.
  2. For each step in `state.steps` (in order): look up `StepState` in `state.step_states`, map to status string, build `WorkflowStepView { name, status, agent, model, depends_on }`.
  3. Append a pseudo-step for each entry in `state.teardown_step_states` (if non-empty): `depends_on: [name_of_last_step_in_state.steps]`.
  4. Return `WorkflowViewState { steps, current_step: state.current_step_index }`.
- `StepState` → status string mapping: `Pending` → "pending", `Running { .. }` → "running", `Succeeded` → "done", `Failed { .. }` → "error", `Cancelled` → "cancelled", `Skipped` → "skipped"
- `PhaseStepStatus` → status string: same mapping as above

**CLI `--follow` step output** (in Layer 2 `RemoteCommand` or Layer 3 CLI frontend):
- Track last-seen `HashMap<String, StepState>` (and `Vec<PhaseStepState>` for setup/teardown)
- On each poll that returns a changed `WorkflowState`, print only lines where status changed, in the format shown in WI 0078's "Remote Follow Mode" section
- Setup steps prefixed with `[setup]`, main steps with `[step N]`, teardown steps with `[teardown]`


## Edge Case Considerations

- **Clone failure**: If `GitEngine::clone_repo` fails (network error, invalid URL, authentication failure), `CreateSessionCommand` must return an error. No session record is written to the database. The Layer 3 handler returns HTTP 422 with the git error message.
- **Branch checkout failure**: If `checkout_or_create_branch` fails after a successful clone (e.g. the branch name is invalid), the session creation fails. The partially-cloned repo directory must be cleaned up (call `git_engine.delete_directory(cloned_path)` in the error path of `CreateSessionCommand`). Do not leave orphaned directories under `~/.awman/sessions/`.
- **Session kill with running jobs**: `DELETE /sessions/{id}` returns HTTP 409 Conflict. The response body includes the list of running job IDs. The client is responsible for waiting or cancelling jobs.
- **Remote session kill — partial cleanup**: If `delete_directory` fails (e.g. permissions issue), log the error and return HTTP 500. The session is NOT marked as closed — the operator must resolve the filesystem issue and retry.
- **`local` session with non-existent workdir**: When creating a `local` session, validate that the supplied `workdir` path exists and is a directory. Return HTTP 400 if not. This check happens in `CreateSessionCommand`, not in the Layer 3 handler.
- **Session restore on server restart**: On startup, sessions in `active` status are restored from `ApiDb`. For `remote` sessions, verify that `cloned_path` still exists on disk. If the path is missing (e.g. host rebooted and temp storage was lost), mark the session as `closed` with an error note rather than leaving it in an invalid state.
- **Concurrent `clone_repo` calls**: Each remote session has a unique UUID-based `cloned_path`, so concurrent clone operations target different directories and do not interfere with each other.
- **`remote session start` command**: The CLI `awman remote session start` (defined in WI 0078) must expose `--type`, `--workdir` (for `local`), `--repo-url`, and `--branch` (for `remote`) flags. These are registered in the `CommandCatalogue` for the `remote session start` subcommand.
- **Worktree suppression — audit**: All existing callers in `ExecWorkflowCommand` that create git worktrees must check `session.session_type.is_remote()`. There must be no code path that creates a worktree for a remote session.
- **API vs CLI/TUI sessions**: `JobQueue`, `SessionType`, and the `CreateSessionCommand` / `KillSessionCommand` are API-specific concerns. CLI and TUI create `Session` objects directly without going through `CreateSessionCommand`. The `SessionType` enum lives in Layer 0 but CLI/TUI sessions should always be `Local` — enforce this with a constructor that requires a `workdir` for non-API sessions.
- **Workflow state file written before visible in API**: `WorkflowEngine` writes state after each step transition. The first write happens when the first step enters `Running` status. A client polling `GET /sessions/{id}/jobs/{job_id}/workflow` immediately after job submission may receive HTTP 404 even though the job is `running` — this is expected. Clients must tolerate 404 during the initial lag.
- **Workflow state file for completed jobs**: After the workflow completes and the job is marked `completed`, the workflow state file persists in `job_state_dir`. It is NOT deleted on session kill (only the remote session's `repo/` directory is deleted). Operators can inspect historical workflow state for completed jobs.
- **Workflow state schema version mismatch in API response**: If the `WorkflowState` JSON was written by an older version of awman with a different schema version, the deserialization may fail. Return HTTP 500 with the schema mismatch message. Do not silently return a partially-deserialized state.
- **Per-job `Engines` copy**: `Engines` is cloned per-job by `QueueWorker` with only the `WorkflowStateStore` replaced. All other engine references (ContainerRuntime, GitEngine, etc.) are `Arc`-shared and do not incur extra cost. Ensure `Engines` derives or manually implements `Clone` to support this pattern cleanly.
- **TUI strip for prompt jobs**: When a `remote exec prompt` job is submitted, the TUI should NOT start `RemoteWorkflowPoller` — there is no workflow state to show. Instead, show a simple "running" indicator in the tab and update to "done" or "failed" when job status resolves.


## Test Considerations

- **Local session creation test**: `POST /sessions` with `type: local, workdir: /tmp/test-repo`; assert session record created with `session_type: Local`, `working_dir == /tmp/test-repo`.
- **Remote session creation test** (requires network or mock `GitEngine`): `POST /sessions` with `type: remote, repo_url: ..., branch: main`; assert `cloned_path` exists on disk and session record is created.
- **Remote session — branch exists test**: If the specified branch exists in the remote, assert `BranchDisposition::CheckedOut` is returned and the branch is checked out in the clone.
- **Remote session — branch not found test**: If the branch does not exist remotely, assert `BranchDisposition::Created` and the branch is created locally.
- **Remote session kill test**: Kill a `remote` session; assert `cloned_path` directory is deleted from disk and session is marked `closed`.
- **Clone failure cleanup test**: If `clone_repo` succeeds but `checkout_or_create_branch` fails, assert that `cloned_path` is cleaned up and no session record exists in the database.
- **409 on session delete with running job test**: While a job is in `running` status, attempt `DELETE /sessions/{id}`; assert HTTP 409 is returned with the running job ID in the body.
- **Server restart — remote session missing dir test**: Insert a `remote` session in `active` status with a nonexistent `cloned_path`; simulate server startup; assert the session is moved to `closed` with an error note.
- **ExecWorkflowCommand no-worktree test**: Execute a workflow against a `remote` session; assert `GitEngine::create_worktree` is never called (use a mock `GitEngine`).
- **Atomic claim test**: Spawn 4 worker tasks against a queue with 4 pending jobs; assert each job is claimed by exactly one worker.
- **Stale job recovery test**: Insert a job with `status = 'running'` and `claimed_at` older than the timeout; run `JobQueue::recover_stale_jobs()`; assert status reset to `pending`.
- **Worker count config test**: Set `workers: 0`; assert no workers are spawned and startup emits a warning.
- **Layer 0 unit tests**: `JobQueue::enqueue`, `claim_next`, `complete_job`, `fail_job`, `list_by_session`, `get_job` — tested in isolation against an in-memory SQLite database.
- **Workflow state file path test**: `api_paths::job_workflow_state_path(session_id, job_id)` returns the expected path. Unit test — no filesystem I/O.
- **QueueWorker writes workflow state test**: Execute a mock `exec_workflow` job via a `QueueWorker` backed by a mock `WorkflowEngine`. Assert that `workflow_state.json` exists at `job_state_dir` after execution.
- **Workflow status endpoint — file not yet written test**: Call `GET /sessions/{id}/jobs/{job_id}/workflow` on a freshly-enqueued job (no workflow state file yet); assert HTTP 404.
- **Workflow status endpoint — file present test**: Write a fixture `WorkflowState` JSON to `job_state_dir`, then call the endpoint; assert HTTP 200 and the body deserializes correctly.
- **Workflow status endpoint — prompt job test**: Submit an `exec_prompt` job; assert `GET /sessions/{id}/jobs/{job_id}/workflow` returns HTTP 404 with the expected error message.
- **`workflow_state_to_view_state` unit test**: Construct a `WorkflowState` with 3 main steps (one running, one done, one pending), 2 setup steps (both done), 1 teardown step (pending); call the conversion; assert output has 6 `WorkflowStepView` entries in order (2 setup + 3 main + 1 teardown) with correct status strings.
- **TUI strip poller integration test**: Submit a remote exec workflow job from a mock TUI session; assert `RemoteWorkflowPoller` is started, polls the workflow endpoint, and updates `workflow_view` with the correct `WorkflowViewState`.
- **TUI strip final state test**: When job transitions to `completed`, assert the poller does one final poll, updates the view to reflect all steps as "done", then stops.


## Codebase Integration

- Strictly follow `aspec/architecture/2026-grand-architecture.md`. `SessionType`, job queue types (`JobRecord`, `JobQueue`, `JobId`, etc.), and path helpers for remote session directories all live in Layer 0. `GitEngine` clone/checkout/delete methods live in Layer 1. `CreateSessionCommand`, `KillSessionCommand`, `QueueWorker`, and `QueueWorkerFrontend` live in Layer 2. Layer 3 only exposes HTTP routes and spawns worker tasks.
- `SessionType` is a Layer 0 type. The decision to skip worktree creation based on `SessionType::is_remote()` is made in Layer 2 (`ExecWorkflowCommand`). `WorkflowEngine` in Layer 1 must NOT be aware of session types — it must not receive session type information or branch on it.
- `JobQueue` is passed to `QueueWorker` as a shared handle (`Arc<JobQueue>`) so multiple worker tasks share the same connection pool.
- `QueueWorker` must implement the `DispatchFrontend` supertrait pattern via `QueueWorkerFrontend` — no special code path in `Dispatch` for workers.
- New SQLite tables must be added as proper migrations in the `ApiDb` schema versioning system.
- `workers` config field lives in `GlobalConfig` (Layer 0). Spawning worker tasks is a Layer 3 server startup concern.


## Documentation

After implementation:
- `docs/08-api-mode.md` — add a "Sessions" section explaining `local` vs `remote` session types, creation request bodies, `branch_disposition` in the response, and session lifecycle (clone on create, delete on kill)
- `docs/08-api-mode.md` — add a "Job Queue" section: how to submit jobs, poll status, worker configuration
- `docs/07-configuration.md` — document the `workers` global config option
- Create `docs/11-api-sessions-and-jobs.md` as a user guide: end-to-end example for both session types, submitting jobs, polling results
