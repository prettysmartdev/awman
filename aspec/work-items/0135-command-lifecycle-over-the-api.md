# Work Item: Feature

Title: Command lifecycle over the API: cancel, labels, idempotency and one-shot runs
Issue: issuelink

## Summary:
- API clients cannot stop a running command. `DELETE /v1/sessions/{id}` cancels queued commands but lets the running one finish. `agents kill` (0128) is the nearest thing, but it has to be submitted through another session and looks like a step failure, so it can trigger `on_failure` retries.
- Clients also cannot tag their work. After a client restart the only way to find "my run for node X" is a command id the client saved itself. A retried `POST` creates a duplicate.
- Running one job takes four calls and a polling loop: create session, poll status until `ready`, post command, delete session.
- Add a per-command cancel, client labels with an idempotency key, and a one-shot `POST /v1/runs`.

## User Stories

### User Story 1:
As a: team member, through infact

I want to:
cancel a node that is running the wrong thing

So I can:
stop spending agent time on it right away, not when it finishes

### User Story 2:
As a: API client restarting after a deploy

I want to:
list `GET /v1/commands?label=infact.run=<id>` and reattach

So I can:
recover in-flight work without keeping a second copy of awman's state

### User Story 3:
As a: API client

I want to:
`POST /v1/runs {session: {...remote spec...}, command: {...}, close_on_finish: true}` and get back `{session_id, command_id}`

So I can:
start a job in one call, then just follow its logs

## Implementation Details:
- `POST /v1/commands/{id}/cancel`:
  - Queued: mark the command `cancelled`.
  - Running: stop its containers (the same path as `agents kill`), mark the workflow state `aborted` without running `on_failure`, run teardown with `cancelled=true` (so no push or PR), and set the status to `cancelled`.
  - Responses: 202 while stopping, 200 once already terminal.
- Labels: `labels: {string: string}` (max 16 pairs, 256 bytes each) on sessions and commands, stored in the API database, returned on every read and filterable on `GET /v1/sessions` and the new `GET /v1/commands?label=k=v&status=`.
- Idempotency: an `Idempotency-Key` header on `POST /v1/sessions`, `/v1/commands` and `/v1/runs`. The same key within 24 h returns the original response.
- `POST /v1/runs`: create the session, queue the command to start once setup reaches `ready` (no 409), and close the session after a terminal status when `close_on_finish` is set. Setup failure marks the command `error` with `result.error = setup_error`. This replaces the client's `wait_ready` loop.

## Edge Case Considerations:
- Cancel during setup (`/v1/runs`): abort setup, delete the clone, mark the command `cancelled`.
- A cancelled command must not be re-queued on restart.
- Teardown when cancelled: skip push and PR steps, keep cleanup steps.

## Test Considerations:
- Cancel queued, cancel running (fake runtime), cancel after done; no `on_failure` retry after cancel.
- Label filter queries; idempotency replay; `/v1/runs` happy path, setup failure and `close_on_finish`.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/09-api-and-remote-mode.md`.
