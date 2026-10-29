# Work Item: Feature

Title: Structured command results, outputs, and step-attributed live output over the API
Issue: issuelink

## Summary:
An API client today has to rebuild most of what a command did by scraping the event stream:

- **Results.** A command's `result` holds `exit_code`, an `error` string and a few fields (`stage`, `error_status`, `pushed`, `worktree_path`, `setup_error`). Everything else has to be scraped:
  - the branch, from the text "Worktree created: <path> (branch: <b>)";
  - the PR URL, from teardown stdout;
  - failed steps, from transitions;
  - the agent's answer for `exec prompt`, by printing sentinel markers and parsing stdout;
  - the JSON from `agents status` and similar commands, from a `StdoutLine`.
- **Final status.** No `command_status` event can be trusted as the final word:
  - it is not emitted for `exec prompt`, for non-exec commands, or for failures before the engine starts;
  - the workflow's event is emitted before push-back, and a push failure then emits a second one;
  - a teardown failure is reported as `command_status: done` with exit code 1, while the command row says `error`.
- **Output.** A workflow's output is one undifferentiated stream. `StdoutLine(String)` and `StderrLine(String)` carry no step, container or retry, so a client cannot tell which agent of a parallel group printed a line. The TUI avoids this by reading each container's PTY directly (`container_view.rs`, `Ctrl-S` to cycle the focused slot).
- **Timing.** Non-interactive agents (`claude --print`) produce their output as one final message when the container exits, not live. In a real infact run, the `docs` step's whole output (22 lines) arrived after its work was done, while `tests` was still running.
- **Step numbering.** `step_index` in events is the order in which the API frontend first saw each step (`ApiDispatchFrontend::step_index_for`). It is not the step's position in the workflow, and it restarts with each attempt, so it disagrees with `steps[]` in `GET /v1/workflows/{id}`.

This item gives each command one structured `result`, which is also the payload of exactly one final `command_status` event. It adds an outputs directory that awman collects and serves. It attaches a `source` to every event, giving the step, phase, container and retry it came from, and gives streaming-capable agents a live activity feed. Together these let a client build the equivalent of the TUI's Workflow Overview and container panes without parsing log text.

(This item absorbs the former proposal "Step-attributed, live output for API clients".)

## User Stories

### User Story 1:
As a: API client

I want to:
read `GET /v1/commands/{id}/status` and get `result.outcome`, `result.failure`, `result.git` (branch, base and head commits, changed files), `result.pushed`, `result.pull_request` and `result.failed_steps`

So I can:
record what a run produced without parsing human-readable log lines

### User Story 2:
As a: API client using `exec prompt` for a planning agent

I want to:
have the agent write `/awman/output/answer.json` and fetch it from `GET /v1/commands/{id}/outputs/answer.json`, or read `result.final_message`

So I can:
drop stdout sentinels and parsing heuristics

### User Story 3:
As a: API client following the SSE stream

I want to:
receive exactly one `command_status` event, carrying the same `result` object as the status route, as the last event before `done`

So I can:
finish handling a command from the stream alone, without a follow-up poll

### User Story 4:
As a: API client rendering a running workflow (e.g. infact's workflow view, infact work item 0002)

I want to:
see which step, container and retry produced every output line, receive `step_activity` events while an agent works, and read `stages` and per-step `status` from `GET /v1/workflows/{id}`

So I can:
show each agent's output live, and switch between the parallel agents of a group the way the TUI does

## Implementation Details:

### 1. The result object
`CommandResult` (`src/data/fs/api_db.rs`) is the single description of how a command ended. The status route returns it, the final `command_status` event carries it, and 0147's WSP `result` embeds it unchanged. Every new field is optional and filled only where it is known. Existing fields keep their meaning.

```jsonc
{
  "exit_code": 1,
  "outcome": "error",            // NEW: done | paused | aborted | error | cancelled (= CommandStatusKind)
  "failure": {                   // NEW: present unless outcome is done
    "stage": "teardown",         // post-admission stages: setup | preflight | workflow_prerequisites | step | teardown | merge | push
    "code": "step_failed",       // snake_case, from CommandError::code()
    "message": "Step 'tests' failed",
    "step": "tests"              // when a single step is to blame
  },
  "failed_steps": [              // NEW: every failed main, setup or teardown step
    { "name": "tests", "index": 2, "phase": "main", "exit_code": 1, "step_attempt": 2, "error": "…" }
  ],
  "git": {                       // NEW: commands that ran in a git checkout
    "branch": "awman/work-item-0137",   // branch holding the result: merge target after merge-back, else the kept worktree's branch
    "base_sha": "…",             // HEAD when the command started
    "head_sha": "…",             // HEAD of `branch` at the end, after merge-back and push
    "dirty": false,              // uncommitted changes remain in the checkout
    "changed_files": [ { "path": "src/x.rs", "status": "modified", "added": 10, "deleted": 2 } ],
    "changed_files_truncated": false
  },
  "pushed": { "remote": "origin", "branch": "…", "sha": "…" },   // unchanged (0133)
  "pull_request": { "url": "https://github.com/o/r/pull/7" },   // NEW
  "worktree_path": "…",                                          // unchanged (0134)
  "final_message": "…",          // NEW: exec prompt, from the agent's structured output (section 4)
  "outputs": { "files": 2, "bytes": 5120 },                      // NEW: summary of /awman/output (section 3)
  "data": { },                   // NEW: commands with a --json form (section 1, below)

  "error": "Step 'tests' failed",   // kept: equals failure.message
  "error_status": 500,              // kept, deprecated: use failure.code
  "stage": "teardown",              // kept, deprecated: equals failure.stage
  "setup_error": { }                // kept, deprecated: setup failures use failure.stage = "setup"
}
```

- **`outcome`** reuses `CommandStatusKind`, the enum clients already match in `command_status` events, so there is no third status vocabulary. The command row's `status` stays the coarse `done | error | cancelled`, and `outcome` refines it (an `error` row has `outcome` `error`, `aborted` or `paused`).
- **`failure`** replaces the mix of an `error` string that is sometimes a code (`"cancelled"`, `"setup_error"`) and sometimes a message, an HTTP status stored in an asynchronous result (`error_status`), and a `stage` that is set for only two failures. To produce it:
  - add `CommandError::code()` next to `stage()` and `http_status()` in `src/command/error.rs`, with one snake_case code per variant (`worktree_exists`, `merge_conflict`, `merge_target_dirty`, `workflow_prerequisites_unresolved`, `unknown_agent`, `agent_untrusted`, `step_failed`, `teardown_failed`, `push_failed`, `setup_failed`, `cancelled`, `internal`);
  - extend `stage()` to cover every failure.

  The legacy fields stay and are filled exactly as today, under the API's additive-versioning rule.
- **Teardown failure** is `outcome: "error"` with `failure.stage: "teardown"`. Today the engine reports `CommandStatusKind::Done` with exit code 1 (`command_frontend.rs`, `WorkflowOutcome::CompletedTeardownFailed`). The row and the event will no longer disagree.
- **`failed_steps`** are objects, not names, so they carry the phase, the retry and the error without another lookup. `index` is the step's position in the workflow definition (see section 5).
- **`git.changed_files`** compares `base_sha` with the final working tree, not just `base..head`, so uncommitted edits count. That matters for `exec prompt` in a local session without `push`, which leaves its work uncommitted.
  - It is built from `git diff --numstat -M <base>` plus `git ls-files --others --exclude-standard` (untracked files: `status: "added"`, no line counts).
  - The index is never modified.
  - The list is capped at 500 entries, with `changed_files_truncated: true` when the cap is hit.
  - For a remote session, compute it before teardown deletes the clone.
- **`pull_request.url`** is captured by the `create_pull_request` teardown step from `gh pr create` output. That is the only place the URL is parsed. It is an object so `number` and `state` can be added later without another field.
- **`data`**: commands whose CLI has a `--json` form (`agents status|list|detail|kill`, and any catalogue command with a JSON presenter) put that JSON value in `result.data`. They also keep emitting it as a `StdoutLine` for one release. Clients stop parsing JSON out of a log line.

### 2. One final `command_status` event
- The queue worker emits **one** `command_status` for **every** command, including `exec prompt`, non-exec commands, failures before the engine starts, setup failures in `POST /v1/runs`, and cancellations. It is the last event before `Done`, and it is emitted **after** the terminal row is written. Its payload gains `result`, the same object the status route returns:

  ```
  CommandStatus { status, exit_code, error, result: Option<CommandResult> }
  ```

  The status route and the stream are then two views of one value and cannot disagree, because the event is built from the row.
- The engine's own completion report (`ApiDispatchFrontend`, `defer_workflow_completion` / `emit_deferred_command_status`) stops emitting `CommandStatus`. It keeps its `workflow_phase_transition` for `main` and hands its status to the queue worker. The push-failure `CommandStatus` in `queue_worker.rs` goes away: push failure is just a `failure.stage = "push"` in the one final event.

### 3. Outputs directory
- Mount a per-command directory read-write at `/awman/output` into the agent containers of `exec prompt` and of every workflow step, including the dynamic leader. It is stored as `<API root>/sessions/<session>/commands/<command>/output/`, under the API root and never inside the clone, so it survives a remote session's clone being deleted.
- Routes:
  - `GET /v1/commands/{id}/outputs` returns `{files: [{path, size, sha256, modified_at}]}`;
  - `GET /v1/commands/{id}/outputs/{path}` returns the file, with `Content-Type` guessed from the extension and `application/octet-stream` otherwise.
  - Both routes work while the command is running.
  - Retention and deletion follow the command's logs.
- **Serving is hostile-input safe.** The agent controls this directory, so:
  - reject `..`, absolute paths and empty segments in `{path}`;
  - list and serve **regular files only**, opened without following symlinks (`O_NOFOLLOW` on every component, or `openat2(RESOLVE_BENEATH)` on Linux, plus a canonical-prefix check elsewhere). A symlink planted by the agent must never expose a host file (`aspec/architecture/security.md`).
  - Symlinks, devices and FIFOs are omitted from the listing.
- **Limits.** A file over 16 MiB is listed, but fetching it returns 413. The listing and `result.outputs` report the total. The directory has no hard quota, because it is the same disk exposure as the agent writing into the workspace. Document that.
- **Attempts.** Outputs follow the work:
  - `exec prompt` reruns from scratch after a restart, so its directory is cleared at the start of each attempt;
  - a workflow resumed from saved state keeps the files of the steps it keeps, and a rerun step overwrites its own files.
- **Parallel steps** share the one directory. Name collisions are last-writer-wins, and the step prompt tells agents to write under a subdirectory named after their step when they run in parallel.
- The `exec prompt`, leader and step system prompts gain one line naming `/awman/output` as the place for machine-readable results.

### 4. Agent output formats: live activity and the final message
- Add a capability to `AgentDefinition`: `output_format = "text" | "claude_stream_json" | "codex_json"`. It is a property in the agent table, not a name match. Under the API (non-interactive), an agent with a structured format runs in that mode, e.g. `claude --output-format stream-json --verbose`.
- One translator per format (`src/engine/agent_output/`) turns the agent's stream into three things:
  - `StepActivity { kind: "message" | "tool_use" | "tool_result" | "thinking", summary }` events while the agent works. `summary` is one line of at most 200 characters, never the full tool payload, which bounds event volume.
  - The agent's final message, emitted as ordinary output lines exactly as today, so text-only clients see the same output.
  - The same final message as `result.final_message` for `exec prompt`.

  This one translator is the source of both the final message and the activity feed, so there is no second parser.
- Agents with `output_format = "text"` emit only output lines and no `final_message`.
- Translators are tested against recorded fixtures. An unparseable line is passed through as an output line, never dropped.

### 5. Event attribution: `source` on the event envelope
- Add an optional `source` field to the **event envelope** (`ExecutionEvent`, next to `timestamp`, `attempt` and `sequence`), not new output variants:

  ```jsonc
  {
    "timestamp": "…", "attempt": 1, "sequence": 42,
    "source": {
      "phase": "main",              // leader | setup | main | teardown | merge | push
      "step_name": "tests",         // absent for leader and shell setup/teardown output
      "step_index": 2,              // position in the workflow definition
      "container": "awman-…",       // runtime container name, when one exists
      "step_attempt": 1             // 1-based; incremented by on_failure retries
    },
    "payload": { "type": "StdoutLine", "data": "…" }
  }
  ```

- **Why the envelope and not a new `StepOutput` variant.** Existing clients keep working unchanged, because they ignore an unknown field. Output is not emitted twice for a deprecation period. The same attribution also applies to `StatusMessage` ("Container running: …"), `StepActivity` and 0146's terminal events without each defining its own copy. `StdoutLine` and `StderrLine` stay as they are; there is nothing to deprecate.
- **`step_attempt`, not `attempt`.** The envelope's `attempt` already means the command's execution number after a daemon restart (0136). Reusing the name for an `on_failure` retry would make `attempt` mean two things in one JSON object.
- **Where it is set.** `ApiDispatchFrontend::take_io` and `ApiContainerSink` are created per container. Give them the step context the workflow engine already has when it launches a step (the same information as `report_parallel_step_launched`), and stamp it on every event they emit. Workflow-level frontend calls (`report_step_status`, `report_phase_status`) stamp the step or phase they report.
- **`step_index` fix.** `step_index` in `source` and in every existing workflow event (`workflow_step_transition`, `workflow_parallel_step_*`) becomes the step's index in `WorkflowState.steps`. `step_index_for`'s first-seen numbering is removed. The value is then stable across attempts and matches `GET /v1/workflows/{id}`.
- `GET /v1/commands/{id}/logs?step=<name>` filters replay and the live stream to events whose `source.step_name` matches. Cursors stay global (`attempt:sequence`), so a filtered stream resumes with the same `Last-Event-ID`.

### 6. `GET /v1/workflows/{command_id}`: a client-ready view
The route returns the engine's serialized `WorkflowState`. Its statuses are Rust variant names in Serde's externally tagged form (`"Succeeded"`, `{"Running":{"container_id":null}}`, phase `"Main"`), while events use snake_case enums. Add computed fields to the response at read time. They are not added to the persisted state file, which stays an engine format.

- `stages: [[step_name…]…]`: the grouping the TUI's Workflow Overview uses. Extract it into one shared function that both the TUI and the route call.
- Each `steps[i]` gains:
  - `index`;
  - `stage`;
  - `status`: snake_case `StepStatusKind`, the same enum as `workflow_step_transition`;
  - `exit_code` and `error` when failed;
  - `container`;
  - `step_attempt`.
- `current_phase` is accompanied by `phase`, the same value in snake_case.
- `setup_step_states` and `teardown_step_states` entries gain a snake_case `state` alongside the legacy `status`.
- `step_states` and the PascalCase values remain for existing clients. The docs mark them legacy.
- Also serve the route as `GET /v1/commands/{id}/workflow`, so every per-command resource sits under `/v1/commands/{id}/` (`status`, `logs`, `outputs`, 0140's `context-changes`, 0146's `terminals`). The old path stays.

### 7. Feature flags
Append `structured_results`, `outputs`, `event_source` and `step_activity` to `API_FEATURES` (`routes.rs`).

## Edge Case Considerations:
- **Remote sessions** delete the clone on close. Compute `git.*` before teardown removes anything, and keep outputs under the API root.
- **Failures before the engine starts**: admission-time failures already return HTTP 4xx. Failures after admission but before any container starts (unknown agent, prerequisites, existing worktree) get a `result` with `failure.stage = "preflight"` (or `workflow_prerequisites`) and the one final `command_status`. Not every command has a git checkout, so `git` is omitted when there is none.
- **Cancelled commands**: `outcome: "cancelled"`, `failure.code: "cancelled"`. `git` and `outputs` are still filled, because a client wants to see what the cancelled run left behind.
- **Re-run after a restart**: `step_attempt` restarts from the saved state's count. The envelope's `attempt` tells the two runs apart.
- **Leader and shell setup/teardown output**: `source.step_name` is absent and `phase` is set.
- **An `on_failure` retry**: same `step_name` and `step_index`, with `step_attempt` incremented. `failed_steps` lists the step once, with its last attempt.
- **Agents without PTY or streaming support**, or ACP agents: no `step_activity` and no `final_message`. Output lines carry `source` as usual.
- **Very chatty agents**: `step_activity.summary` is one bounded line. Tool payloads never enter the event log.
- **Secrets**: `final_message` and `step_activity.summary` pass through the same redaction as output lines once 0141 lands. Files in `/awman/output` are written by the agent and are not redacted. Document that.
- **Large diffs**: the 500-entry cap and the truncation flag. Binary files report `added`/`deleted` as `null`.

## Test Considerations:
- **Queue worker:**
  - exactly one `command_status` for `exec prompt`, a non-exec command, a pre-engine failure, a `/v1/runs` setup failure, a push failure and a cancel;
  - it is always the last event before `Done`, and its `result` equals the status route's.
- **Results:**
  - `git.*` after a worktree merge-back, after a kept worktree, for uncommitted `exec prompt` edits and for a remote session with `push: true`;
  - `failed_steps` for a retried step and a teardown failure;
  - teardown failure reports `outcome: "error"` in both the row and the event;
  - `pull_request.url` from a recorded `gh pr create` output;
  - `data` for `agents status`.
- **`CommandError::code()`**: every variant maps to a code, and `stage()` is set for every failure path.
- **Outputs routes**:
  - listing, fetch and the 413 size cap;
  - `..` and absolute paths rejected;
  - a symlink to `/etc/passwd` inside the directory is neither listed nor served;
  - cleared for an `exec prompt` re-run, kept for a resumed workflow.
- **Attribution**: a parallel group of two fake agents writing interleaved lines, where every output event carries the right `step_name`, `step_index` and `container`. `step_index` equals the definition index when steps launch out of definition order, and after a restart.
- **Translators**: claude `stream-json` and codex JSON fixtures produce the expected `step_activity` events, the final message lines and `final_message`, and malformed lines pass through.
- **Workflow view**: `stages` matches the TUI overview's grouping for linear, fan-out/fan-in and setup/teardown workflows. The snake_case `status` matches the event enum. `/v1/commands/{id}/workflow` equals `/v1/workflows/{id}`.
- **`?step=` filter**: the live stream and replay are filtered, and resume works with a cursor taken from a filtered stream.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `src/data/fs/api_db.rs` (`CommandResult`), `src/data/execution_event.rs` (envelope `source`, `StepActivity`, `CommandStatus.result`), `src/command/error.rs` (`code()`, `stage()`), `src/command/commands/api_server/{queue_worker,event_bus,push_back}.rs`, `src/frontend/api/{command_frontend,routes}.rs`, the agent table (`AgentDefinition.output_format`), new `src/engine/agent_output/`.
- `awman remote exec --follow` (`src/command/commands/remote.rs`) and remote-bound TUI tabs read the final `command_status` and may prefix lines with `source.step_name`.

## Documentation
- `docs/09-api-and-remote-mode.md`:
  - the result fields and the deprecation of `error_status`, `stage` and `setup_error`;
  - the one final `command_status`;
  - the outputs routes;
  - the event envelope's `source` and `step_activity`;
  - the `?step=` filter;
  - `stages` and the snake_case fields in the workflow view, plus the `/v1/commands/{id}/workflow` alias.
- Fix the polling example's `aborted|paused` case: the row status never takes those values; `result.outcome` does.
