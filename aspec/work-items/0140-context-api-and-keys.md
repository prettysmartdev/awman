# Work Item: Feature

Title: Context over the API, and caller-chosen context keys
Issue: issuelink

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
- Session field `context_key: Option<String>` (regex `^[a-z0-9][a-z0-9._-]{0,127}$`), stored on the session. When it is set, `repo_dir` resolves to `context/repo/_keyed/<key>/` instead of the origin-derived slug. The overlay grammar gains `context(repo@<key>[:ro|rw])` for CLI parity.
- Routes, keyed by scope and key and restricted to `repo` (`global` stays host-private):
  - `GET /v1/context/repo/{key}` returns `{files: [{path, size, sha256, modified_at}]}`.
  - `GET` and `PUT /v1/context/repo/{key}/files/{path}`, with `If-Match: <sha256>` for optimistic concurrency.
  - `DELETE /v1/context/repo/{key}/files/{path}`.
  - Paths are validated with `validate_context_path`; there is a size cap per file and per key.
- Optional: `GET /v1/commands/{id}/context-changes` lists the files the command added, changed or removed in its context scopes (snapshot hashes at start and end).

## Edge Case Considerations:
- Two commands writing the same key concurrently: last writer wins on disk. `context-changes` lets the client detect this; document that `:ro` is the safe default for parallel jobs.
- A key with no directory yet: `GET` returns an empty list, not 404.

## Test Considerations:
- Key resolution (keyed vs inferred); traversal rejected; `If-Match` conflict returns 412; change listing.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/08-overlays.md` (keyed repo context), `docs/09-api-and-remote-mode.md` (context routes).
