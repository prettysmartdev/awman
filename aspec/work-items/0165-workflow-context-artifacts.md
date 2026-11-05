# Work Item: Feature

Title: Read workflow context files after pushed and pulled runs
Issue: issuelink

Depends on: 0146 (context path validation and worker API), 0143 (complete daemon and WSP result). Complete before later integrations depend on workflow notes in remote UIs.

## Summary:
- Workflow steps write notes to `context(workflow)` at `/awman/context/workflow`, under the invocation ID. Final summaries can refer to those notes, but neither push API nor WSP result exposes them. A remote client or work server cannot open them.
- Expose the current attempt's workflow scratch files read-only for the command's retention lifetime, and return bounded artifacts for pulled leases. This is distinct from 0146's durable `context(repo)` memory.

## User Stories

### User Story 1:
As a: API or work-server client showing a completed workflow

I want to:
list and read the notes the workflow cited

So I can:
show the evidence without sharing a filesystem with the awman host

## Implementation Details:
- Add `GET /v1/worker/commands/{id}/context` returning 0139's `{items, next_cursor}` list of `{path, size_bytes, sha256, modified_at}` and `GET /v1/worker/commands/{id}/context/files/{path}` returning bytes. These are read-only worker routes, authorized like `/outputs`. The command record pins its current attempt's `invocation_id`, so a rerun cannot expose another attempt's directory. Commands without a workflow return an empty list.
- Resolve paths beneath that invocation's context directory with 0146's validation. Reject traversal, absolute paths, symlinks and nonregular files; enforce per-file and total size limits. Do not follow a changed symlink between listing and download. Retain the directory for at least as long as the command record, including after `close_on_finish`; delete it with that record under the normal retention policy. Document the concrete retention setting/period in the user API guide.
- Extend WSP `WorkItem` with optional `workflow_context: {max_bytes}` and the lease result with `workflow_context: {items: [{path, size_bytes, sha256, modified_at, content_base64?}], next_cursor: null, truncated: bool}`. Include metadata for every safe file and inline content up to the requested cap; mark `truncated: true` when content is omitted. Artifact overflow does not change the run outcome. A work server may omit the request; extend the reference server (0145) to opt in when it displays notes.
- Advertise `workflow_context` in `capabilities` in `/v1/status` for the push routes, and `lease_workflow_context` in status and WSP `register.features` only when the result attachment is supported. A server offers the requested attachment only to a capable worker; mismatch releases `ineligible` before work.

## Edge Case Considerations:
- An in-flight command lists its current files; files removed before download return 404 with `not_found`. A failed or cancelled command retains safe notes through the same record lifetime.
- A restarted daemon preserves command-to-invocation mapping. Missing directories for old records yield an empty list with a clear retention explanation in docs.
- A lost lease epoch cannot submit artifacts; never include host paths in WSP or API responses.

## Test Considerations:
- API list/download for in-flight, complete, failed and `close_on_finish` commands; empty list for a command with no workflow; current-attempt isolation on rerun and restart.
- Traversal, symlink, replacement race and size limits fail closed. Verify list shape, metadata hashes and capability flag.
- WSP result includes bounded files, marks omitted content explicitly and omits artifacts for a lost lease. Reference server conformance covers an attached note.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 0 owns safe context file metadata, path validation and command-to-invocation persistence; Layer 1 owns bounded file reads; Layer 2 owns command/lease authorization, retention and WSP result assembly; Layer 3 maps the worker routes. Reuse 0146's context file utilities without conflating durable repo memory and per-invocation scratch.

## Documentation

- Update `docs/08-overlays.md`, `docs/09-api-and-remote-mode.md` and `docs/17-workers-and-work-servers.md` with access and retention behavior.
