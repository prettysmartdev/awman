# Work Item: Bug

Title: Use one host Git transport policy for remote sessions and classify authentication failures
Issue: issuelink

Depends on: 0143 (shared lease-side Git transport builder and complete daemon gate). Complete before 0147 adds push-session credentials.

## Summary:
- A remote run can clone with one credential and fail to push its successful result with another. `src/engine/git/host_command.rs` disables system gitconfig for clone/setup, while `src/engine/git/remote_push.rs` currently reads it for transport `ls-remote` and push. A stale system credential helper, such as macOS `osxkeychain`, can win during push-back.
- Apply one host-side Git transport command builder to clone, setup fetch, merge-ref `ls-remote`, push-back `ls-remote`, and push for every session. Use the same config and credential scope on each operation. Report Git authentication failures with a stable code.

## User Stories

### User Story 1:
As a: client submitting a remote run

I want to:
have the successful commit published with the same Git identity that cloned the repository

So I can:
trust a completed run and distinguish an expired credential from a remote rejection

## Implementation Details:
- Reuse the transport builder introduced for lease operations in 0143. All host-side transport commands disable system gitconfig, as clone does today, while retaining the invocation's allowed global/user Git configuration. Apply the same safety controls for hooks, filters, fsmonitor, signing and maintenance already required by `aspec/architecture/security.md`. No operation may silently use a different helper chain. The builder accepts an optional session or lease credential source; WI0147 adds the push-session one-shot source through this same input.
- Keep URLs free of credentials. Do not expose the selected helper, token or password in argv, events, diagnostics or `.git/config`. Document the no-system-config policy in `aspec/architecture/security.md` and `docs/09-api-and-remote-mode.md` when implemented.
- Classify authentication failure from clone, fetch, `ls-remote` or push as `failure.stage: "git_auth"`, `failure.code: "git_auth_failed"`, matching 0143's lease result. The message names the operation and a sanitized remote identifier, never a credential. Preserve separate codes for other push failures and for WI0164's unreachable remote. Add `git_auth_failure` to `capabilities` in `/v1/status` when push-session classification ships.
- Apply the builder to both pushed sessions and pulled leases; this item audits the push side and closes any parity gap left by 0143. No compatibility route or credential mode is added.

## Edge Case Considerations:
- An explicit user credential helper may fail or refuse a request; report `git_auth_failed` without falling back to system Git configuration.
- A remote URL with embedded credentials is rejected or sanitized before Git is launched or persisted, following the existing remote-session policy.
- `git_auth_failed` is for an authenticated transport operation, while a missing ephemeral credential after daemon restart remains 0147's `credentials_lost` admission failure.

## Test Considerations:
- Smart-HTTP regression: system config helper yields a stale password, allowed global config yields the correct one; clone, setup fetch, `ls-remote` and push use the correct identity and succeed. Repeat for a lease clone/push through the same builder.
- Force auth failure at each transport operation and assert `git_auth_failed`, operation/remote naming, and no secret in argv, `.git/config`, events or result. Assert unrelated push rejection keeps a different code.
- Verify the status capability is absent before the behavior is available and present afterward.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 0 owns typed transport options and safe remote identifiers; Layer 1 `src/engine/git/` owns the one builder, process configuration and auth-error classification; Layer 2 supplies session/lease credential scope and maps transport failures into `CommandResult`; Layer 3 only serializes the result. The builder must not import HTTP or command modules.

## Documentation

- Update `aspec/architecture/security.md` for the host Git configuration policy and `docs/09-api-and-remote-mode.md` for result codes and client recovery. Keep implementation details in this item.
