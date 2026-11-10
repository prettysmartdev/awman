# Work Item: Feature

Title: Consistent host Git transport, push retry, per-session credentials and per-command secrets
Issue: issuelink

Depends on: 0143 (shared lease-side Git transport builder and complete daemon gate).

## Summary:
- A remote run can clone with one credential and fail to push its successful result with another. `src/engine/git/host_command.rs` disables system gitconfig for clone/setup, while `src/engine/git/remote_push.rs` currently reads it for transport `ls-remote` and push. A stale system credential helper, such as macOS `osxkeychain`, can win during push-back.
- Apply one host-side Git transport command builder to clone, setup fetch, merge-ref `ls-remote`, push-back `ls-remote`, and push for every session. Use the same config and credential scope on each operation. Report Git authentication failures with a stable code.
- A brief connection refusal, DNS failure, timeout or reset during push-back currently fails a successful run; `close_on_finish` can then delete the clone holding its result commit. Use the shared transport path to retry only failures before a remote response, with a bounded window and a distinct exhausted-transport code. Apply the same policy to push sessions and lease pushes; 0143 already requires the lease-side minimum.
- API clients serving multiple teams need to provide short-lived Git credentials per session and secret values per command. URL-embedded tokens leak into logs and Git configuration; one broad host credential cannot enforce repository scope. Keep caller-provided values in memory only.

## User Stories

### User Story 1:
As a: client submitting a remote run

I want to:
have the successful commit published with the same Git identity that cloned the repository

So I can:
trust a completed run and distinguish an expired credential from a remote rejection

### User Story 2:
As a: client awaiting a completed remote run

I want to:
have awman recover from a brief Git server restart without rerunning the agents

So I can:
receive the result commit or a precise failure I can handle

### User Story 3:
As a: API client with a GitHub App installation token for one repository

I want to:
pass `git_auth: {kind: "token", username: "x-access-token", token: "..."}` when creating the session (`POST /v1/worker/sessions`)

So I can:
let awman clone and push that repository only, without the token appearing in any URL or log

### User Story 4:
As a: API client

I want to:
pass `secrets: {"GITHUB_TOKEN": "..."}` on a command (`POST /v1/worker/commands`) and use `env(GITHUB_TOKEN)` in it

So I can:
give each project its own token for `gh pr create` without restarting awman

## Implementation Details:
- **Land transport and authentication first, push retry second, and credential/secret delivery third.** Audit and repair push-session clone, fetch, `ls-remote` and push parity, and ship stable authentication classification as the first independently reviewable slice. The fix does not wait for retry or secret plumbing.
- Reuse the transport builder introduced for lease operations in 0143. All host-side transport commands disable system gitconfig, as clone does today, while retaining the invocation's allowed global/user Git configuration. Apply the same safety controls for hooks, filters, fsmonitor, signing and maintenance already required by `aspec/architecture/security.md`. No operation may silently use a different helper chain. The builder accepts an optional session or lease credential source; this item adds the push-session one-shot source through this same input.
- Keep URLs free of credentials. Do not expose the selected helper, token or password in argv, events, diagnostics or `.git/config`. Document the no-system-config policy in `aspec/architecture/security.md` and `docs/09-api-and-remote-mode.md` when implemented.
- Classify authentication failure from clone, fetch, `ls-remote` or push as `failure.stage: "git_auth"`, `failure.code: "git_auth_failed"`, matching 0143's lease result. The message names the operation and a sanitized remote identifier, never a credential. Preserve separate codes for other push failures and the unreachable-remote retry below. Add `git_auth_failure` to `capabilities` in `/v1/status` when push-session classification ships.
- Apply the builder to both pushed sessions and pulled leases; this item audits the push side and closes any parity gap left by 0143. The new `git_auth` field uses the existing session-creation route; no compatibility route is added.
- **Push retry:** centralize push transport classification and retry policy in the host Git path used by `push_after_success` and lease publication. Retry only connection refused, DNS, connect/read timeout before a response, and connection reset before a response. Use capped exponential backoff with a small attempt/window limit; bound the whole wait to seconds and to the command deadline. Authentication, hook rejection, non-fast-forward and any other remote response are terminal on the first attempt.
- Emit an execution event for every retry containing attempt number, operation, delay in `_ms` and sanitized remote, without Git stderr or credentials. Advertise `push_retry_events` for push-session events. Exhaustion yields `failure.stage: "push"`, `failure.code: "push_unreachable"`, reusing 0143's lease result code; authentication keeps `git_auth_failed`, and remote refusal keeps `push_failed`. Advertise `push_retry_codes` when push-session classification ships. Both flags belong in `/v1/status` `capabilities` and are independent of the later credential and secret flags.
- Before a retry, use the same builder to check whether the remote branch already points at the intended `head`; if so, report `pushed: true` and the published head. A failed check caused by the same unreachable remote remains retryable; a definitive different head follows the normal conflict/refusal path.
- Respect cancellation during backoff and report `cancelled`. For leases, keep heartbeats flowing and stop retries before lease expiry; an expired/lost lease never pushes again. `close_on_finish` runs only after retry and final result recording. Do not introduce persistent retry state outside the command/lease record.
- Session `git_auth` (token or basic) is kept in the runtime's in-memory session map and never persisted. If awman restarts, the session's next Git operation fails with `credentials_lost`, and the client opens a new session. Push-session clone, setup fetches, merge-ref `ls-remote`, push-back `ls-remote` and push all use the shared builder with a one-shot credential helper (`git -c credential.helper=` plus an awman helper reading a private socket or fd). The secret never lands in argv, the environment or `.git/config`; `origin` keeps the clean URL. The lease-side helper already ships in 0143.
- Teardown steps that run Git (`push_branch`, `pull_branch`, `create_pull_request`) inside containers get the same helper through a mounted socket that answers only for the session's host.
- `secrets` on `POST /v1/worker/commands` (and `POST /v1/worker/runs`) are kept in memory, injected as container env for names the command's overlays request with `env(NAME)`, and take precedence over host env. Values are redacted (exact-match replacement with `***`) from every event payload that carries agent text: `StdoutLine`, `StderrLine`, `StatusMessage`, `StepActivity.summary`, and `result.final_message`/`failure.message` (0137). Redaction lives in one place, the event bus sender, so a new event type cannot bypass it. Files the agent writes to `/awman/output` and terminal bytes (0141) are not redacted; document that.
- Validate at most 32 secrets of 8 KiB each; names must match `^[A-Z_][A-Z0-9_]*$`.
- Add `session_git_auth` and `command_secrets` to `capabilities` in `/v1/status` when their respective push fields and delivery paths work. WSP `lease_credentials` retains the same capability name defined in 0143.

## Edge Case Considerations:
- An explicit user credential helper may fail or refuse a request; report `git_auth_failed` without falling back to system Git configuration.
- A remote URL with embedded credentials is rejected or sanitized before Git is launched or persisted, following the existing remote-session policy.
- `git_auth_failed` is for an authenticated transport operation, while a missing ephemeral credential after daemon restart is this item's `credentials_lost` admission failure. Token expiry mid-run reports `git_auth_failed`, so the client can retry with a fresh token.
- A reset after the remote accepted a push may leave an uncertain outcome; the branch-head check resolves it before another push. Never repeat a non-idempotent publish step without checking. A branch advancing to a different commit during retry is a conflict, not an unreachable success.
- A lease heartbeat that cannot renew ends retry before fencing; the result never claims a push by a lost epoch.
- A restarted command whose secrets were lost fails fast instead of running without them.

## Test Considerations:
- Smart-HTTP regression: system config helper yields a stale password, allowed global config yields the correct one; clone, setup fetch, `ls-remote` and push use the correct identity and succeed. Repeat for a lease clone/push through the same builder.
- Force auth failure at each transport operation and assert `git_auth_failed`, operation/remote naming, and no secret in argv, `.git/config`, events or result. Assert unrelated push rejection keeps a different code.
- Verify the status capability is absent before the behavior is available and present afterward.
- Smart-HTTP server fails before any response once, then recovers: one retry event and a successful push. Permanent pre-response failure exhausts into `push_unreachable` after a bounded interval.
- A server accepts the push then drops the connection: head check reports success without a second push. Auth, hook and non-fast-forward replies are never retried. Cancellation during backoff returns `cancelled`.
- Repeat the transport and cancellation cases for pushed commands and leases, with a short lease expiry. Check retry capability flags, error codes and secret-free events.
- Clone over HTTP with a test server that requires basic auth. Assert no secret in event logs, `git config` or `ps` output; verify redaction, scope, expiry and restart failures.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md` and `aspec/architecture/security.md`: Layer 0 owns typed transport options, safe remote identifiers, non-persisted secret values and validated names; Layer 1 owns the one Git builder, process configuration, auth-error classification, head checking, bounded retry mechanics, credential-helper transport, runtime injection and exhaustive event redaction before publication; Layer 2 owns session/lease credential scope, per-session/per-command lifetime, precedence, admission policy, cancellation, retry event/result policy, lease heartbeat/epoch limits and mapping transport failures into `CommandResult`; Layer 3 parses typed API requests and serializes results/events. The helper and event bus must not import HTTP or command modules. One policy serves push sessions and leases, and one Layer 2 command path serves worker push and the equivalent builtin/daemon execution where applicable.

## Documentation

- Update `aspec/architecture/security.md` for host Git configuration and secret handling, `docs/09-api-and-remote-mode.md` for credentials, secrets, push result codes, retry behavior and recovery, `docs/17-workers-and-work-servers.md` for lease push retry, and `docs/04-security-and-isolation.md` for secret delivery and redaction. Keep algorithm details in this item.
