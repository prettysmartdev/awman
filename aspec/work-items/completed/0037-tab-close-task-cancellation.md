# Work Item: Task

Title: Cancel Long-Running Tasks and Docker Processes on Tab Close
Issue: issuelink

## Summary:

When a tab is closed mid-execution, long-running Docker subprocesses launched via `run_container_captured` continue running in the background even though no tab is listening to their output. Add explicit cancellation so that closing a tab during a non-PTY Docker run kills the underlying subprocess and frees the Tokio task.

## User Stories

### User Story 1:
As a: user

I want to:
close a tab and have the associated Docker container stop immediately

So I can:
free system resources (CPU, memory, Docker containers) without having to manually run `docker kill`

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Area 4.2).

### Current Behaviour

**PTY path (interactive):** When a tab is closed, `TabState` is dropped, which drops `PtySession`. Dropping `PtySession` drops the PTY master, which sends SIGHUP to the child process (`docker run`). The container stops. This path is **correct**.

**Non-PTY captured path:** `run_container_captured` calls `std::process::Command::output()` inside a `tokio::spawn` task. There is no mechanism to kill the Docker subprocess when the tab is closed. The task and Docker container continue until the container exits naturally.

### Proposed Fix

1. **Store a task handle in `TabState`:** Add `text_command_handle: Option<tokio::task::JoinHandle<()>>` to `TabState`.

2. **Abort the task on tab close:** In `App::close_tab`, before removing the tab, call `tab.text_command_handle.take().map(|h| h.abort())`.

3. **Kill the Docker subprocess:** Aborting a `spawn_blocking` task does not kill the subprocess it is waiting on. To kill the Docker container, also store the container name in the tab and send `docker kill <name>` when the tab is closed. The container name is already generated via `docker::generate_container_name()` and available at launch time.

4. **Name all captured-output containers:** Some `run_container_captured` calls currently pass `None` for `container_name`. To enable kill-on-close, assign a generated name to each container launch and store it in `TabState`.

### Alternative (simpler, partial fix)

If the full kill-on-close is out of scope, at minimum abort the Tokio task on tab close to free the worker thread. The Docker container will continue running until it exits naturally, but the Tokio side is cleaned up.

### PTY path (no change needed)

PTY-based container sessions already clean up correctly via SIGHUP on master PTY close. No change needed for the `PtySession` path.

## Edge Case Considerations:
- If `close_tab` is called during the brief window between task spawn and container start, the `docker kill` may fail (container not yet running). This is safe — the kill simply fails with a non-zero exit code, which should be ignored.
- If the Docker daemon is unreachable, `docker kill` will fail. This is also safe — the container will be cleaned up by Docker's own restart/cleanup logic or will exit when the daemon reconnects.
- Multi-phase workflows (ready → rebuild): the tab may be in a phase where the container name changes between phases. Store the current container name per phase or use a `Vec` of active container names.

## Test Considerations:
- Integration test: launch a non-PTY Docker command that sleeps for 60s, close the tab, verify the container is no longer running (`docker ps --filter name=<name>`).
- Unit test: mock the JoinHandle abort and verify it is called on `close_tab`.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Primary files: `src/tui/state.rs` (TabState), `src/tui/mod.rs` (App::close_tab, spawn sites).
- Coordinate with Work Item 0036 (spawn_blocking) since that work item changes how tasks are structured.
