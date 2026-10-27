# Work Item: Bug

Title: Resumable, unambiguous command event streams
Issue: issuelink

## Summary:
Several behaviours of `GET /v1/commands/{id}/logs` force clients to cross-check every stream end against the status endpoint:

- After 300 s waiting for a worker, the stream sends `event: done` even though the command is still queued (`routes.rs:1047-1052,1107-1112`).
- SSE frames carry no `id:`, so `Last-Event-ID` resume is impossible.
- A command re-queued after a restart truncates `events.log` (`api_command_log.rs:33`) and restarts `sequence` at 0, so a client cannot tell the new attempt from the old one.
- Possibly (unverified): the stdout drain runs as a separate task, so the final lines can land after `Done` and be lost from both the stream and the log.

## User Stories

### User Story 1:
As a: API client following a long-queued command

I want to:
keep a single stream open until the command really ends, and resume it after a disconnect without replaying everything

So I can:
treat `event: done` as the end of the command

## Implementation Details:
- Waiting for a worker: keep sending comment keepalives (every 15 s instead of 250 ms) until the job is claimed or the client disconnects. Never send `Done` for a command that is not terminal. If a time limit stays, end with `event: timeout` instead.
- Each frame sends `id: <attempt>:<sequence>`. Honour `Last-Event-ID` and `?after=<attempt>:<sequence>`.
- Attempts: add `attempt` (starting at 1) to the command row and to each event. A re-queue increments `attempt` and appends to `events.log` instead of truncating it. Emit `StatusMessage{phase:"queue", message:"re-queued after restart (attempt N)"}`.
- Ordering: join the stdout/stderr drain tasks before emitting the final `CommandStatus` and `Done`.

## Edge Case Considerations:
- Old clients that ignore `id:` keep working.
- A `Last-Event-ID` from an attempt that is no longer kept: replay from the start of the current attempt.

## Test Considerations:
- A queued command beyond 300 s does not end the stream; resume by id; a re-queue increments `attempt`; the last stdout line comes before `Done` in a stress test.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/09-api-and-remote-mode.md`: the event stream section.
