# Work Item: Feature

Title: Context over the API, and caller-chosen context keys
Issue: issuelink

Depends on: 0143 (the complete daemon and finalized config/API surface).

## Summary:
`context(repo)` is a good home for durable agent memory. It mounts a persistent directory and injects instructions telling the agent to read and update it. API clients cannot use it well, for three reasons:

- The directory sits on the awman host, and no route reads or writes it.
- The key is inferred from the `origin` URL (`context_dirs.rs:81`, `remote_slug.rs`). A local checkout cloned from a filesystem path lands in `_local/<dirname>`, while a remote session on the same repo over HTTP lands in `<owner>/<repo>`. The git host is ignored, so `github.com/a/b` and `gitlab.com/a/b` collide.
- Several awman hosts each keep their own copy.

## User Stories

### User Story 1:
As a: API client running a planning agent and execution agents on the same project (e.g. infact)

I want to:
pass `context_key: "infact-<project-id>"` on every session and get the same `/awman/context/repo` in all of them

So I can:
give every agent one shared project memory regardless of how the repo was cloned

### User Story 2:
As a: API client that is the system of record for memory

I want to:
`PUT` the memory files before a command and `GET` what the agent changed afterwards

So I can:
show memory in my UI, let people edit it, back it up, and sync it across awman hosts

## Implementation Details:
- Session field `context_key: Option<String>` on `worker` push sessions (`POST /v1/worker/sessions`) (regex `^[a-z0-9][a-z0-9._-]{0,127}$`), stored on the session. When it is set, `repo_dir` resolves to `context/repo/_keyed/<key>/` instead of the origin-derived slug. The overlay grammar gains `context(repo@<key>[:ro|rw])` for CLI parity.
- Routes. These are core daemon routes (0139 §5), shared by every feature and keyed by context key. Only `repo` context is exposed; `global` stays host-private, so the scope is not a path segment:
  - `GET /v1/contexts/{key}` returns 0139's list shape: `{items: [{path, size_bytes, sha256, modified_at}], next_cursor}`.
  - `GET` and `PUT /v1/contexts/{key}/files/{path}`, with `If-Match: <sha256>` for optimistic concurrency.
  - `DELETE /v1/contexts/{key}/files/{path}`.
  - Paths are validated with `validate_context_path`; there is a size cap per file and per key.
- `GET /v1/worker/commands/{id}/context_changes` returns 0139's list shape, `{items: [{path, change: "added"|"modified"|"deleted", size_bytes?, sha256?}], next_cursor}`, for files the command added, changed or removed in its context scopes (snapshot hashes at start and end).
- **Pulled jobs:** extend 0143's WSP `WorkItem` with optional `context: {files: [{path, content_base64}], max_bytes}`. The work server owns this per-item snapshot. After path and size validation, the worker stages it as that lease's `context(repo)`; it never reuses a host context key or carries state from another lease. The WSP result adds `context_changes: {items: [{path, change: "added"|"modified"|"deleted", size_bytes?, sha256?, content_base64?}], next_cursor: null}` for changed files, with content inlined up to `max_bytes` and deletion represented without content. If changed content exceeds the cap, report an explicit `context_too_large` failure instead of silently dropping it. The server applies returned changes to its own store before the next lease. The worker needs no inbound API access from the server.
- Advertise `context_api` in `capabilities` in `/v1/status` for keyed push context/routes, and `lease_context` in status and WSP `register.features` only when pulled context staging and returned changes are supported. A server offers context-bearing items only to workers advertising `lease_context`; a mismatch is released as `ineligible` before execution. WI0165 handles the separate `context(workflow)` scratch files.

## Edge Case Considerations:
- Two commands writing the same key concurrently: last writer wins on disk. `context_changes` lets the client detect this; document that `:ro` is the safe default for parallel jobs.
- A key with no directory yet: `GET` returns an empty list, not 404.
- A malformed, symlinked, duplicate or traversal path in a pulled context snapshot is rejected before the lease starts. Size limits apply to decoded bytes. A failed/cancelled lease returns any safe context changes with its partial result under the same cap; a lost lease epoch cannot publish changes.

## Test Considerations:
- Key resolution (keyed vs inferred); traversal rejected; `If-Match` conflict returns 412 with `code: precondition_failed`; change listing; responses follow 0139 (list shape, `size_bytes`, `_at` timestamps).
- WSP conformance: two workers on distinct hosts receive the same server-owned context, changed and deleted files return to the server, and the next lease observes the update; oversized/malformed snapshots and unsupported workers fail closed. Assert both capability flags.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 0 owns `ContextKey`, path validation, file metadata and atomic context storage; Layer 2 owns session key resolution, scope/authorization, concurrency rules and the typed context operations; Layer 3 maps the core HTTP routes to those operations. The API adapter does not resolve keys, validate filesystem policy or read context files directly. Keep the one overlay grammar and one session context model across CLI, TUI and API.

## Documentation
- `docs/08-overlays.md` (keyed repo context), `docs/09-api-and-remote-mode.md` (context routes).
