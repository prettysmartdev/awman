# Work Item: Feature

Title: Daemon network listener hosting: listen address, certificate names, capacity and key rotation
Issue: issuelink

Depends on: 0145 (one daemon, one API, features; the network listener), 0159 (naming rules, config tree, route tree).

## Summary:
The API server is shaped for one developer on one machine:

- It binds `127.0.0.1` with no option (`api_server.rs:383`).
- Its certificate covers only `127.0.0.1` and `localhost` (`auth/mod.rs:559-560`), so `::1` and real hostnames fail verification.
- `/v1/status` omits the worker count and the global queue.
- There is exactly one key, and `--refresh-key` replaces it in one step, so rotation means downtime.

Running awman next to a coordination server in docker compose, or as one host among several, needs a tunnel today. After 0145 the API server is the daemon's **network listener**. This item gives that listener a configurable address, verifiable certificates, capacity in `/v1/status`, and named keys.

## User Stories

### User Story 1:
As a: operator running awman in a container next to its client

I want to:
`awman config set daemon.network.listen 0.0.0.0:9876`, `awman config set daemon.network.tlsSans awman.internal` and `awman config set daemon.network.enabled true`

So I can:
reach it over the compose network with TLS that verifies

### User Story 2:
As a: client scheduling across several awman hosts

I want to:
read `max_concurrent_jobs`, running and queued counts per feature, and `oldest_queued_age_ms` from `/v1/status`

So I can:
route work to the least-loaded host

### User Story 3:
As a: operator

I want to:
add a second named key, move the client to it, then delete the first

So I can:
rotate credentials without downtime

## Implementation Details:
- **Where this lives after 0145.** The network listener serves the features in `daemon.network.features` (default `["worker"]`, whose push side is today's API). Its settings are rows in 0159's key table, all `global` scope, set with `awman config set` (0159 §4):

  | Key | Default | Live | Meaning |
  |---|---|---|---|
  | `daemon.network.enabled` | `false` | yes | Open the network listener. |
  | `daemon.network.listen` | `"127.0.0.1:9876"` | no (restart) | `host:port` to listen on. `[::]:9876` and IPv6 literals are accepted. |
  | `daemon.network.tlsSans` | `[]` | yes (regenerates the certificate) | Extra DNS names and IPs for the generated certificate. |
  | `daemon.network.features` | `["worker"]` | yes | Features served on this listener (0145). |
  
- **No old surface.** There are no start flags and no compatibility spellings:
  - `awman api start --bind/--tls-san`, `--dangerously-skip-tls` and `--refresh-key` are removed;
  - the old `api.*` keys are unknown keys (0159).
- **TLS only.** The network listener is always TLS, and there is no plaintext option. Plaintext loopback use was the only reason for `--dangerously-skip-tls`, and the daemon's local listener (Unix socket or named pipe, 0145) now covers local clients.
- **Listen address.**
  - The effective address is logged and shown in `awman daemon status`.
  - The local listener is unaffected by `listen`.
  - `listen` is not live: `awman config set daemon.network.listen …` persists the value and says `restart the daemon to apply`, as 0159 specifies. Toggling `daemon.network.enabled` *is* live: it opens or closes the network listener without touching the local listener or running work.
- **Certificate names.** `tlsSans` entries are added to the generated certificate's SANs, and `::1` is always included. Changing the SANs regenerates the certificate, reloads it into the running listener, and says so.
- **`/v1/status` and `/v1/health`** are core daemon routes (0145, 0159 §5), present whatever features are on.
  - `/v1/status` includes:
    - `version`;
    - `features` (per feature: `state`, `max_concurrent_jobs`, `running`, `queued`, `oldest_queued_age_ms`);
    - the host-wide `max_concurrent_jobs`;
    - `agents` (installed agent names);
    - `listeners` (local, and network with its `listen` address and served features).
  - An unauthenticated `GET /v1/health` returns only `{status}`, for load balancers.
- **Keys** are the network listener's API keys, separate from the daemon's owner key (0145). They are the core resource `/v1/keys` (0159 §5), owner key only, on the local listener:
  - `POST /v1/keys {name}` → `{id: "key_…", name, key, created_at}`, with `key` shown only in this response;
  - `GET /v1/keys` → `{items: [{id, name, created_at, last_used_at}], next_cursor}`, never key material;
  - `DELETE /v1/keys/{id}`.
  
  The friendly actions are `awman daemon api keys.create name=<name>`, `keys.list` and `keys.delete id=<id>` (0159 §6). Hashes are stored in `<data>/daemon/api_keys.json` (0600). There is no `api key` command group and no `--refresh-key`. "Rotate everything" is `keys.create` followed by `keys.delete` for each old key.
- **Trace propagation.** The push routes `POST /v1/worker/sessions`, `POST /v1/worker/commands` and `POST /v1/worker/runs` accept a `traceparent` header. It is stored on the row and included in the queue status event, which lets clients correlate their traces with awman logs.

## Edge Case Considerations:
- Listening on a non-loopback address inside a container is the common case. Document that the Docker socket mount is the real trust boundary.
- `daemon.network.features` must never expose an ephemerally enabled feature (0145), however it is configured. Listing `engine` with a non-loopback `listen` logs a warning at startup.
- A listen failure fails only the network listener. The local listener and the features keep running, and `awman daemon status` shows the error with the address it tried. The persisted value stays, so the user can correct it and restart.
- Deleting the last key leaves the network listener up but rejecting every request, and `awman daemon status` says so.
- `tlsSans` entries that are neither a valid DNS name nor an IP literal are refused by the key table, and nothing is persisted.

## Test Considerations:
- `daemon.network.listen` parsing (IPv4, IPv6, `[::]`, a missing port refused). Setting it reports that a restart is needed, and a restart listens on the new address.
- `daemon.network.enabled` toggles the listener live, and the local listener stays up throughout.
- SANs in the generated certificate, and live regeneration on change.
- `/v1/status` fields and their 0159 naming (`_ms` durations, per-feature objects).
- `/v1/keys`:
  - create returns the key once;
  - list never returns key material and uses `items`/`next_cursor`;
  - delete revokes at once;
  - IDs carry `key_`;
  - an API key cannot call `/v1/keys`.
- The `keys.*` actions match their `--raw` equivalents.
- Unauthenticated health.
- Old `api.bind` in a config fails validation as an unknown key, with the moved-key hint.
- A network listen failure leaves the local listener up.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, especially `aspec/architecture/security.md`.
- **Config:** the `daemon.network.*` keys are rows in `src/data/config/fields.rs` within the daemon section struct (`src/data/config/daemon.rs`, 0159). The network listener (`src/frontend/api/serve.rs`) applies them.
- **Routes and commands:**
  - `/v1/keys` is a core route in the merged router (0145);
  - the `keys.*` entries go in the client-side action table, `src/command/commands/daemon/actions.rs` (0159 §6);
  - remove the `api` catalogue group (`src/command/dispatch/catalogue/api.rs`).

## Documentation
- `docs/16-daemon.md`: the network listener, with `daemon.network.*` set through `awman config set`, keys through `/v1/keys` and `awman daemon api keys.*`, and TLS-only.
- `docs/09-api-and-remote-mode.md`: the same, from the client's side. Remove `awman api start` flags, `api key` and `--refresh-key`. Also correct the docs' session discriminator to `session_type`, and show that a command's status comes from `GET /v1/worker/commands/{id}` (the `/status` alias is gone, 0159).
