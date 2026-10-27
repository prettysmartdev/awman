# Work Item: Feature

Title: Structured command results and outputs over the API
Issue: issuelink

## Summary:
- A command's result is only `{exit_code, error?}`. Everything else an API client needs has to be scraped from the event stream:
  - the branch, from the text "Worktree created: <path> (branch: <b>)";
  - the PR URL, from teardown stdout;
  - failed steps, from transitions;
  - for `exec prompt`, the agent's answer, by printing sentinel markers and parsing stdout.

  `CommandStatus` events are not emitted for `exec prompt` or for failures before the engine starts, and are emitted before the API-mode merge.
- Return a structured `result`, and give every command an outputs directory that awman collects and serves.

## User Stories

### User Story 1:
As a: API client

I want to:
read `GET /v1/commands/{id}/status` and get `result.verdict`, `result.branch`, `result.head_sha`, `result.pushed`, `result.pr_url`, `result.failed_steps` and `result.changed_files`

So I can:
record what a run produced without parsing human-readable log lines

### User Story 2:
As a: API client using `exec prompt` for a planning agent

I want to:
have the agent write `/awman/output/answer.json` and fetch it from `GET /v1/commands/{id}/outputs/answer.json`

So I can:
drop stdout sentinels and parsing heuristics

## Implementation Details:
- `CommandResult` grows the following optional fields, each filled where known:
  - `verdict` (`done|paused|aborted|error`)
  - `branch`, `base_sha`, `head_sha`
  - `pushed` (bool, see 0133)
  - `pr_url`, captured by `create_pull_request` from `gh pr create` output; this is the one place the URL is parsed
  - `failed_steps[]`
  - `changed_files` (`git diff --numstat base..head`, capped at 500 entries)
  - `final_message`, the agent's last message for `exec prompt`, when the agent reports one: claude `--output-format json`, codex last message
- The queue worker emits a `CommandStatus` event for every subcommand, as the last event before `Done`, after the merge and push have finished.
- Outputs: mount a per-command directory at `/awman/output` (rw) into agent containers for `exec prompt` and every workflow step. Serve it with `GET /v1/commands/{id}/outputs` (a listing) and `GET /v1/commands/{id}/outputs/{path}`, with a 16 MiB cap per file, and delete it with the command's logs.
- The `exec prompt` and leader system prompts gain one line naming `/awman/output` as the place for machine-readable results.

## Edge Case Considerations:
- Remote sessions delete the clone on close. Compute `changed_files`/`head_sha` before teardown and keep outputs under the api root, not inside the clone.
- Re-run after a restart: clear the outputs directory at the start of each attempt.
- Paths in the outputs routes: reject `..` and absolute paths.

## Test Considerations:
- Queue-worker tests: `CommandStatus` for `exec prompt` and for a pre-engine failure; result fields after a worktree merge.
- Outputs route tests: listing, fetch, traversal rejected, size cap.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/09-api-and-remote-mode.md`: result fields, outputs.
