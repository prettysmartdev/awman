# Work Item: Task

Title: Wrap Blocking Docker Calls in spawn_blocking
Issue: issuelink

## Summary:

Several call sites invoke `docker::run_container_captured()` and related blocking Docker functions directly inside `tokio::spawn` async tasks without wrapping in `tokio::task::spawn_blocking`. This blocks a Tokio worker thread for the entire duration of the Docker subprocess (potentially minutes during agent runs), starving other async tasks on that thread. Wrap these calls in `spawn_blocking` to release the worker thread while waiting.

## User Stories

### User Story 1:
As a: user

I want to:
have amux remain responsive (TUI updates, input handling) even when a long-running Docker container is running in the background

So I can:
interact with other tabs and commands without UI freezes during multi-minute agent runs

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Area 3.1).

### Problem

`docker::run_container_captured()` calls `std::process::Command::output()` which blocks until the Docker subprocess exits. When called inside a `tokio::spawn` async block, this monopolises a Tokio worker thread. On the default multi-threaded Tokio runtime (thread pool = number of CPU cores), blocking one thread on a long Docker run can starve other concurrent tasks.

The stats poller already uses `spawn_blocking` correctly (mod.rs:1614). These call sites do not.

### Affected Call Sites

All of these call blocking Docker functions from inside `tokio::spawn` tasks (via `spawn_text_command` or direct `tokio::spawn`):

- `src/tui/mod.rs:1116` — audit phase (`run_container_captured`)
- `src/tui/mod.rs:1390` — implement phase (`run_container_captured`)
- `src/tui/mod.rs:1550` — chat phase (`run_container_captured`)
- `src/commands/ready.rs:366` — ready audit (`run_container_captured`)
- `src/commands/ready.rs:663` — ready refresh (`run_container_captured`)
- `src/commands/agent.rs:99` — agent non-interactive run (`run_container_captured`)
- `src/commands/init.rs:212` — init container run (`run_container`)

### Proposed Fix

For each affected call site, wrap the blocking call in `tokio::task::spawn_blocking`:

```rust
// Before
let (_cmd, output) = docker::run_container_captured(image, path, …)?;

// After
let image = image.to_string();
let output = tokio::task::spawn_blocking(move || {
    docker::run_container_captured(&image, &path, …)
}).await??;
```

The double `?` handles: outer `JoinError` (task panic) and inner `anyhow::Error`.

### Alternative: Refactor `spawn_text_command`

An alternative is to modify `spawn_text_command` to accept a synchronous closure and wrap the entire body in `spawn_blocking`:

```rust
pub fn spawn_blocking_command<F>(output_tx: …, exit_tx: …, f: F)
where
    F: FnOnce(OutputSink) -> anyhow::Result<()> + Send + 'static,
```

This is cleaner for call sites that do nothing async except call Docker. However, some callers do mix async and sync operations, so both variants may be needed.

### Note on `run_container` (PTY path)

`docker::run_container` with PTY (the interactive path using `PtySession::spawn`) does NOT have this issue — `PtySession::spawn` uses `std::thread::spawn` for the blocking I/O, and the async code only communicates via channels. This fix only applies to the non-PTY captured-output path.

## Edge Case Considerations:
- `spawn_blocking` tasks cannot capture `&str` or non-`'static` references — all Docker call arguments must be owned (`String`, `PathBuf`, `Vec<…>`). Ensure argument types are cloned before moving into the closure.
- `JoinError` (task panic) should be surfaced as an error, not silently swallowed.
- If the tab is closed while a `spawn_blocking` task is running, the task will continue until the Docker subprocess exits (Docker does not receive a kill signal from `spawn_blocking` abort). This is an existing limitation.

## Test Considerations:
- Verify that the Tokio runtime remains responsive during a long Docker run: spawn a background task that counts ticks while a `spawn_blocking` Docker call runs and confirm tick count is not zero.
- Ensure existing integration tests for `ready`, `implement`, and `chat` still pass after the refactor.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Primary files: `src/tui/mod.rs`, `src/commands/ready.rs`, `src/commands/agent.rs`, `src/commands/init.rs`.
- Do not add new runtime dependencies.
