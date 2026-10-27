# Work Item: Feature

Title: API server hosting: bind address, certificate names, capacity and key rotation
Issue: issuelink

## Summary:
The API server is shaped for one developer on one machine:

- It binds `127.0.0.1` with no option (`api_server.rs:383`).
- Its certificate covers only `127.0.0.1` and `localhost` (`auth/mod.rs:559-560`), so `::1` and real hostnames fail verification.
- `/v1/status` omits the worker count and the global queue.
- There is exactly one key, and `--refresh-key` replaces it in one step.

Running awman next to a coordination server in docker compose, or as one host among several, needs a tunnel today. Small changes would fix that.

## User Stories

### User Story 1:
As a: operator running awman in a container next to its client

I want to:
`awman api start --bind 0.0.0.0:9876 --tls-san awman.internal`

So I can:
reach it over the compose network with TLS that verifies

### User Story 2:
As a: client scheduling across several awman hosts

I want to:
read `workers`, `running_commands`, `queued_commands` and `oldest_queued_age_s` from `/v1/status`

So I can:
route work to the least-loaded host

### User Story 3:
As a: operator

I want to:
add a second named key, move the client to it, then revoke the first

So I can:
rotate credentials without downtime

## Implementation Details:
- `--bind <addr:port>` (also `api.bind` in global config). Any non-loopback address requires TLS: refuse `--dangerously-skip-tls` with a clear error unless `--allow-plaintext-nonloopback` is also given. Log the effective address.
- `--tls-san <name>` (repeatable) is added to the generated certificate's SANs. Always include `::1`. Changing the SANs regenerates the certificate and says so.
- `/v1/status` adds `workers`, `queued_commands` (global), `oldest_queued_age_s`, `agents` (installed agent names) and `version`. Allow an unauthenticated `GET /v1/health` that returns only `{status}`, for load balancers.
- Keys: `awman api key add <name>`, `awman api key list`, `awman api key revoke <name>`. Store hashes in `api_keys.json`. `--refresh-key` stays as "revoke all and add `default`".
- Trace propagation: accept a `traceparent` header on `POST /v1/sessions`, `/v1/commands` and `/v1/runs`; store it on the row and include it in `StatusMessage{phase:"queue"}`. This lets clients correlate their traces with awman logs.

## Edge Case Considerations:
- Binding non-loopback inside a container is the common case; document that the Docker socket mount is the real trust boundary.
- Squad and the API server are still mutually exclusive; the error message should name the bound address.

## Test Considerations:
- Bind refusal rules; SANs in the generated certificate; status fields; key add, revoke and auth; unauthenticated health.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, especially `aspec/architecture/security.md`.

## Documentation
- `docs/09-api-and-remote-mode.md`. Also fix two places where it disagrees with the code: the session discriminator is `session_type` (the docs say `type`), and the command route is `/v1/commands/{id}/status` (the docs show the bare path).
