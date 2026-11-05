# Work Item: Feature

Title: Per-session git credentials and per-command secret values
Issue: issuelink

Depends on: 0143 (the complete daemon and finalized worker push API).

## Summary:
Remote sessions clone with the awman host's own git and ssh setup, and `env(NAME)` can only forward variables from the awman process environment. An API client serving many teams therefore has two bad options:

- embed tokens in `repo_url`, where they show up in clone logs and `git remote -v` inside containers;
- or give the awman host one broad credential for every repository.

Let the caller hand awman short-lived credentials per session and secret values per command. Hold them in memory only.

## User Stories

### User Story 1:
As a: API client with a GitHub App installation token for one repository

I want to:
pass `git_auth: {kind: "token", username: "x-access-token", token: "..."}` when creating the session (`POST /v1/worker/sessions`)

So I can:
let awman clone and push that repository only, without the token appearing in any URL or log

### User Story 2:
As a: API client

I want to:
pass `secrets: {"GITHUB_TOKEN": "..."}` on a command (`POST /v1/worker/commands`) and use `env(GITHUB_TOKEN)` in it

So I can:
give each project its own token for `gh pr create` without restarting awman

## Implementation Details:
- Session `git_auth` (token or basic) is kept in the runtime's in-memory session map and never persisted. If awman restarts, the session's next git operation fails with `credentials_lost`, and the client opens a new session.
- Push-session clone, setup fetches, merge-ref `ls-remote`, push-back `ls-remote` and push all use the one shared host Git transport builder required by 0143 and WI0163. It applies one Git configuration (no system gitconfig) and a one-shot credential helper (`git -c credential.helper=` plus an awman helper that reads from a private socket or fd) to every transport operation. The secret never lands in argv, the environment or `.git/config`; `origin` keeps the clean URL. The lease-side helper is already required by 0143; this item extends it to push sessions.
- Teardown steps that run git (`push_branch`, `pull_branch`, `create_pull_request`) inside containers get the same helper through a mounted socket that answers only for the session's host.
- `secrets` on `POST /v1/worker/commands` (and `POST /v1/worker/runs`): kept in memory, injected as container env for names the command's overlays request with `env(NAME)`, and they take precedence over host env. Values are redacted (exact-match replacement with `***`) from every event payload that carries agent text: `StdoutLine`, `StderrLine`, `StatusMessage`, `StepActivity.summary`, and `result.final_message`/`failure.message` (0137). Redaction lives in one place, the event bus sender, so a new event type cannot bypass it. Files the agent writes to `/awman/output` and terminal bytes (0141) are not redacted; document that.
- Validation: max 32 secrets of 8 KiB each; names must match `^[A-Z_][A-Z0-9_]*$`.
- Add `session_git_auth` and `command_secrets` to `capabilities` in `/v1/status` when their respective push fields and delivery paths work. WSP `lease_credentials` retains the same capability name defined in 0143.

## Edge Case Considerations:
- Token expiry mid-run: git fails with an auth error. Surface it as `result.failure.stage = "git_auth"` with `code: git_auth_failed` (0137 and 0139 shapes), the same code used for ordinary Git authentication failures by WI0163, so clients can retry with a fresh token.
- A restarted command after an awman restart has lost its secrets: fail fast with a clear error instead of running without them.

## Test Considerations:
- Clone over HTTP with a test server that requires basic auth; no secret in the events log, `git config` or `ps` output; redaction test.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md` and `aspec/architecture/security.md`: Layer 0 owns typed, non-persisted secret values and validated names; Layer 1 owns the credential-helper transport, runtime injection and exhaustive event redaction before publication; Layer 2 owns per-session/per-command lifetime, scope, precedence and admission policy; Layer 3 only parses typed API requests and renders errors. The helper and event bus must not import HTTP or command modules. One Layer 2 command path must serve worker push and the equivalent builtin/daemon execution where applicable.

## Documentation
- `docs/09-api-and-remote-mode.md`, `docs/04-security-and-isolation.md`.
