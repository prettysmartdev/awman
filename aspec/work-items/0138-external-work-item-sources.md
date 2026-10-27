# Work Item: Feature

Title: Work items from external sources: HTTP provider, `--issue` in dynamic mode, inline bodies
Issue: issuelink

## Summary:
- Today the only way to give dynamic mode its input is a file named `NNNN-*` in the repository (`dynamic.rs:279-299`). `--issue` exists but dynamic mode rejects it, and GitHub is the only `IssueSource` (`src/engine/issue/router.rs`). API clients that keep specs elsewhere must commit a file onto a branch just so awman can read it, and their numbers can collide with work items already in the repo.
- Add a generic HTTP issue provider, accept `--issue` in dynamic mode, and accept an inline work item on `POST /v1/commands`.
- Make `NNNN` resolution deterministic: today `read_dir` order decides which of two `0001-*` files wins (`issue.rs:11-49`). Error when more than one file matches.

## User Stories

### User Story 1:
As a: API client that stores specs in its own database (e.g. infact)

I want to:
run `exec workflow --dynamic --issue https://infact.example/api/projects/P/nodes/N/work-item`

So I can:
stop committing a work item file onto a special branch for every run

### User Story 2:
As a: user with work items in the repo

I want to:
get a clear error when two files match `--work-item 0007`

So I can:
avoid running the wrong spec silently

## Implementation Details:
- `HttpIssueSource`: claims inputs that start with a configured base URL (global config `issueSources.http: [{ baseUrl, tokenEnv }]`). It sends `GET <url>` with `Accept: application/json` and `Authorization: Bearer $tokenEnv`. The response is `{ "id": "...", "title": "...", "body": "markdown", "number": 42? }`, mapped onto `Issue`. A `text/markdown` response is also accepted, with the title taken from the first heading. The provider sits after GitHub in the router, so bare integers keep their current meaning.
- Dynamic mode with `--issue`: build `WorkItemContext` from the fetched issue (the same temp-file overlay as `execute.rs:157-217`). Key the dynamic-workflow and resume state by `issue.numeric_id()` when present, else by a short hash of `source_id`. `--issue` and `--work-item` stay mutually exclusive.
- Inline: `POST /v1/commands` accepts an optional `work_item: { title, body, id? }`. awman writes it to the same temp file and treats it like `--issue`. This covers callers that cannot expose an HTTP endpoint the awman host can reach.
- Resolution: `find_work_item_file` collects every match and returns `AmbiguousWorkItem { number, paths }` when there is more than one.

## Edge Case Considerations:
- Fetch failures (401/404/timeout) fail the command at preflight with the provider error, before any container starts.
- The token is never logged, and the URL is logged without its query string.
- A resumed run whose issue body has changed: resume uses the saved workflow; log that the spec changed since the workflow was designed.

## Test Considerations:
- Router tests (HTTP base-URL match, GitHub still owns integers); provider tests against a local test server (JSON, markdown, 401, 404).
- Dynamic preflight with `--issue` and with an inline work item; resume keying by hash.
- Ambiguous `NNNN` error.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/06-dynamic-workflows.md` (inputs), `docs/10-github-integration.md` (issue sources), `docs/09-api-and-remote-mode.md` (inline work item).
