# Work Item: Task

Title: API consistency pass: error codes, resource layout, session readiness, and dead surface
Issue: issuelink

## Summary:
An audit of the whole HTTP API covered the routes as built (through 0136 and 0149) and the pending proposals (0137–0148). Most of it holds together: sessions, commands, runs, labels, idempotency, cancellation and resumable SSE compose well. The structured-result and event-attribution findings are fixed in 0137. This item collects the remaining inconsistencies. Each one is small, but together they make clients match strings, make extra round trips, or read documentation that contradicts the server.

1. **Errors have no machine-readable code.** Every HTTP error is `{"error": "<text>"}`. Clients have to match prose, and the prose varies for the same condition:
   - `/v1/commands/{id}/status` answers 404 with `"Command '<id>' not found"`, while `/v1/workflows/{id}` answers `"command not found"`;
   - the workflow route's four 404 meanings (`command not found`, `no workflow for this command`, `workflow not started`, `workflow never started`) are documented as exact strings to compare.

   Proposals add their own spellings: 0145 `mode_disabled`, 0139 412 on `If-Match`, 0140 `credentials_lost`.
2. **Per-command resources are split across two roots.** `/v1/workflows/{command_id}` is the only per-command resource outside `/v1/commands/{id}/`, where `status`, `logs`, `cancel`, and the proposed `outputs` (0137), `context-changes` (0139) and `terminals` (0146) sit. Meanwhile `GET /v1/commands/{id}` (the record itself) does not exist: the record is at `/status`, which 0141 notes the docs already get wrong. Sessions have both `/sessions/{id}` (the record) and `/sessions/{id}/status` (setup progress), so `/status` means different things on the two resources.
3. **A session's record hides whether it can be used.** `GET /v1/sessions` and `GET /v1/sessions/{id}` return `status: active|closing|closed` but not `setup_status`, although the row stores it (`SessionResponse` in `routes.rs`). `?status=active` therefore includes sessions whose clone or merge failed. The only way to tell is one extra `GET /sessions/{id}/status` per session.
4. **The docs contradict the server on admission.** The "API edge cases" table says an unknown flag in `args` is enqueued (202) and fails later at the worker. The "Malformed `args`" section, and the code, return 400 at admission. The "Submit a command" section says `exec`'s `args` "starts with the exec action", but every example puts the action in `subcommand` (`"exec prompt"`).
5. **A dead, misleading schema projection.** `CommandCatalogue::rest_route_table()` and `openapi_schema()` (`src/command/dispatch/projections/api_schema.rs`) are used only by their own tests. They describe `POST /v1/<command path>` routes (e.g. `/v1/exec/workflow`) that the server has never served.
6. **The `pending` status is legacy but still part of the contract.** `pending` appears in the command status table, the `status` filter and the cancel table as a "legacy spelling of `queued`". Every client must handle two names for one state.
7. **`log_path` leaks a server path that is useless to clients.** Every command response carries `log_path: "~/.awman/api/sessions/…/output.log"`. A remote client cannot open it, and the field discloses the server's layout. The logs route is the client's interface.
8. **The session is chosen by a header on one route and by the body on another.** `POST /v1/commands` takes the session from `x-awman-session`. `POST /v1/runs` nests it in the body, and every other per-resource route uses the path. A header that names a resource is easy to forget (it is a documented 400), and it is invisible in most HTTP client logs.
9. **There is no rule for `features`.** Each item appends to `API_FEATURES` ad hoc. Nothing says when a feature name must be added, so clients cannot rely on its absence meaning "unsupported".

## User Stories

### User Story 1:
As a: API client author

I want to:
branch on `code` in every error body (`command_not_found`, `workflow_not_started`, `session_not_ready`, …)

So I can:
handle errors without matching English text that may change

### User Story 2:
As a: API client

I want to:
find everything about a command under `/v1/commands/{id}`, and see `setup_status` on every session I list

So I can:
navigate the API by one rule and skip a round trip per session

## Implementation Details:
- **Error codes (finding 1).**
  - Every error body becomes `{"error": "<message>", "code": "<snake_case>"}`. `error` keeps its current text (additive).
  - Add one `ApiError { status, code, message }` type in `src/frontend/api/` with constructors per condition, and have `error_json` callers use it. Codes are stable and documented in one table.
  - The asynchronous `result.failure.code` (0137) uses the same vocabulary where conditions overlap (`worktree_exists`, `merge_conflict`, …).
  - 0139, 0140, 0141 and 0145 use this shape for their new errors. 0147's WSP errors use the same flat shape so one decoder serves both.
  - Initial codes:
    - `unauthorized`, `forbidden_workdir`, `invalid_request`, `unknown_subcommand`, `unknown_flag`;
    - `session_not_found`, `session_closing`, `session_not_ready`;
    - `command_not_found`;
    - `no_workflow`, `workflow_not_started`, `workflow_never_started`;
    - `idempotency_conflict`, `idempotency_in_flight`;
    - `internal`.
- **Resource layout (finding 2).**
  - Add `GET /v1/commands/{id}`, returning the same body as `/status`.
  - Serve the workflow view at `GET /v1/commands/{id}/workflow` (0137 adds it with the new fields).
  - Old paths stay and the docs point to the new ones. No route is removed in v1.
- **Session readiness (finding 3).**
  - Session responses (`SessionResponse`: list, detail, `DELETE`) gain `setup_status` (the stage enum) and, when setup failed, `setup_error: {stage, message}`.
  - `GET /v1/sessions` accepts `setup_status=ready|failed|…` as a filter.
- **Docs fixes (finding 4).** Correct the edge-case row: unknown flags give 400 at admission. Reword the `exec` paragraph to the `subcommand: "exec prompt"` form the examples use. Keep the `args`-first form documented only if the server still accepts it, and add a test either way.
- **Dead surface (finding 5).** Delete `rest_route_table` and `openapi_schema` with their tests. A real schema for the actual routes is a separate, later item. It should be generated from the axum router and the response types, not from the catalogue.
- **`pending` (finding 6).**
  - Stop writing `pending` anywhere, and normalise it to `queued` when reading old rows (one migration or a read-time map in `api_db.rs`).
  - Keep accepting `status=pending` as a filter alias.
  - The docs drop it from the status table.
- **`log_path` (finding 7).** Omit it from responses when the request did not come from loopback, and mark it deprecated. `awman remote` and the TUI do not depend on it; check before removing.
- **Session selection (finding 8).**
  - `POST /v1/commands` also accepts `session_id` in the JSON body.
  - When both the body field and the header are present they must match (400 otherwise).
  - The docs lead with the body form.
  - The header stays supported, and idempotency comparison covers whichever form was sent.
- **Feature rule (finding 9).**
  - Document in `aspec/uxui/cli.md` (API section) and in `docs/09` that every new user-visible route, request field or event type adds a `features` entry in the same change.
  - A test asserts that `features` has no duplicates.

## Edge Case Considerations:
- Clients that match today's `error` strings keep working, because the text is unchanged. Only new conditions get new text.
- `setup_status` on a closed session reports its last stage, which tells a client why a `/v1/runs` session closed early.
- Both a body `session_id` and the header, with different values: 400 `invalid_request`. awman never guesses.
- Old databases with `pending` rows: the claim path already treats them as claimable, and normalisation must not change claim order.

## Test Considerations:
- Route tests: every error path in `routes.rs` returns a `code`, and a table test pins each code to its HTTP status.
- `GET /v1/commands/{id}` equals `/status`; `/v1/commands/{id}/workflow` equals `/v1/workflows/{id}`.
- Session list and detail include `setup_status`; `?setup_status=failed` filters.
- Body `session_id`: accepted, with a conflicting header refused, and an idempotent replay with each form.
- `pending` normalisation on an old database fixture.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `src/frontend/api/routes.rs` (errors, session responses, new routes), `src/data/fs/api_db.rs` (`pending` normalisation), `src/command/dispatch/projections/api_schema.rs` (delete), `tests/api_parity/routes.rs`.
- Coordinate with 0137 (shared failure codes, the `/workflow` alias) and 0141 (its docs fixes overlap finding 2).

## Documentation
- `docs/09-api-and-remote-mode.md`:
  - an error-code table;
  - the `/v1/commands/{id}` and `/workflow` paths;
  - `setup_status` on sessions;
  - body `session_id`;
  - the corrected edge-case rows;
  - `pending` removed from the status table;
  - the `features` rule.
