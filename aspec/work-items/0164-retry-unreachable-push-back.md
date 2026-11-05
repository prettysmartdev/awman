# Work Item: Bug

Title: Retry push-back when the Git remote is briefly unreachable
Issue: issuelink

Depends on: 0143 (complete daemon and lease push baseline), 0163 (one transport builder and auth classification). Complete before 0145's reference work server.

## Summary:
- `push_after_success` currently makes one transport attempt. A connection refusal, DNS failure, timeout or reset during push-back marks a successful run failed; `close_on_finish` then deletes the clone holding its result commit.
- Retry only transport failures that occurred before a remote response, within a short bounded window measured in seconds. Give exhausted transport failures a code distinct from authentication and remote refusal. Apply the same policy to push sessions and lease pushes; 0143 already requires the lease-side minimum.

## User Stories

### User Story 1:
As a: client awaiting a completed remote run

I want to:
have awman recover from a brief Git server restart without rerunning the agents

So I can:
receive the result commit or a precise failure I can handle

## Implementation Details:
- Centralize push transport classification and retry policy in the host Git path used by `push_after_success` and lease publication. Retry only connection refused, DNS, connect/read timeout before a response, and connection reset before a response. Use capped exponential backoff with a small attempt/window limit; bound the whole wait to seconds and to the command deadline. Authentication, hook rejection, non-fast-forward and any other remote response are terminal on the first attempt.
- Emit an execution event for every retry containing attempt number, operation, delay in `_ms` and sanitized remote, without Git stderr or credentials. Give the push-session event a `push_retry_events` status capability. Exhaustion yields `failure.stage: "push"`, `failure.code: "push_unreachable"`, reusing 0143's lease result code; authentication keeps `git_auth_failed`, and remote refusal keeps `push_failed`. Add `push_retry_codes` to `capabilities` in `/v1/status` when push-session classification ships.
- Before a retry, use the same builder to check whether the remote branch already points at the intended `head`; if so, report `pushed: true` and the published head. A failed check caused by the same unreachable remote remains retryable; a definitive different head follows the normal conflict/refusal path.
- Respect cancellation during backoff and report `cancelled`. For leases, keep heartbeats flowing and stop retries before lease expiry; an expired/lost lease never pushes again. `close_on_finish` runs only after retry and final result recording. Do not introduce persistent retry state outside the command/lease record.

## Edge Case Considerations:
- A reset after the remote accepted a push may leave an uncertain outcome; the branch-head check resolves it before another push. Never repeat a non-idempotent publish step without checking.
- A branch advancing to a different commit during retry is a conflict, not an unreachable success.
- A lease heartbeat that cannot renew ends retry before fencing; the result never claims a push by a lost epoch.

## Test Considerations:
- Smart-HTTP server fails before any response once, then recovers: one retry event and a successful push. Permanent pre-response failure exhausts into `push_unreachable` after a bounded interval.
- A server accepts the push then drops the connection: head check reports success without a second push. Auth, hook and non-fast-forward replies are never retried. Cancellation during backoff returns `cancelled`.
- Repeat the transport and cancellation cases for pushed commands and leases, with a short lease expiry. Check capability flags, error codes and secret-free events.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 1 owns transport failure classification, head checking and bounded retry mechanics over WI0163's builder; Layer 2 owns command cancellation, event emission, result policy and lease heartbeat/epoch limits; Layer 3 only exposes events and results. Use one policy for push and pull.

## Documentation

- Update `docs/09-api-and-remote-mode.md` and `docs/17-workers-and-work-servers.md` with the push result codes and retry behavior. Keep algorithm details in this item.
