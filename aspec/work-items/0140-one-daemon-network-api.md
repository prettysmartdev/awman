# Work Item: Feature

Title: Complete single awman daemon, unified API and network hosting
Issue: issuelink

Depends on: 0139 (config and API surface). This item completes the unified process and network listener before 0141–0143 add their features.

## Summary:
- Replace the separate squad and API daemons with one database owner, one feature supervisor and one API. Finish local and network listener hosting, TLS names, capacity reporting and key rotation in this work item.
- Use the 0139 config and route contract throughout. This is the complete daemon foundation for terminal, engine and worker features; no second daemon remains.
- **Acceptance:** squad tasks and pushed API jobs run concurrently in one process and one database; local and network listeners pass their authentication and authorization tests; TLS hostname verification, capacity reporting and named-key rotation work; the old daemon entry points are gone.

## Part: Unified process, features and local API

### Summary:
- Today awman has two long-lived processes, and they refuse to run together:
  - the squad daemon (`awman squad start`);
  - the API server (`awman api start`).

  `DaemonGuard` (`src/engine/daemon/guard.rs`) exists so that only one process ever holds `~/.awman/data/awman.db`. Its error reads "awman api and the squad daemon cannot run at the same time". So a machine can automate with squad tasks *or* serve API clients, never both. Clients work around it with a second awman home (infact's `make dev` does exactly that).
- Replace both with **one daemon process** that owns the database and serves **one API**. Its runtime components are **features**, each enabled and disabled independently, in config or at runtime, without restarting the daemon or the work running under the other features:
  - **`squad`**: today's squad functionality, unchanged: the scheduler, tasks, triggers, evaluations and env sync. It becomes a feature of the one daemon instead of its own daemon.
  - **`worker`**: runs jobs that are **pushed** to it or **pulled** by it.
    - Push is today's API server and `awman remote` job submission.
    - Pull is the Work Server Protocol (0143, infact's pull mode).
    - Both share one job model, one executor share and one event stream.
  - **`engine`**: a singleton awman instance hosted in the daemon. It can drive one or more TUI instances, the Rex integration (0154), and any future awman frontend. Its behaviour and frontend protocol are specified together in 0142. This item provides the feature slot, the local listener it is served on, and ephemeral enablement.
- There are no modes, and no client-specific or Rex-specific features.
- Every config key, route, ID, list, error, duration and CLI verb in this item follows **0139**, which is the reference for the whole surface.
- **Completion of this item:** one daemon owns the database and API, runs squad and today's pushed API work together, exposes an engine feature slot with ephemeral enablement, and hosts the complete network listener specified in Part B. WI141 adds the shared terminal and status foundation, WI142 adds the engine frontends, and WI143 finishes worker pull.
- awman is in beta, so this is a clean break, with no compatibility spellings, aliases or config migrations:
  - The `awman api …` command group and `awman squad start|stop|status|logs` are removed. Squad's task-management subcommands stay; daemon lifecycle and status live in `awman daemon`.
  - Old `api.*`, `squad.*` and `workers` config keys become unknown keys that fail normal config validation, with 0139's moved-key hint.

### User Stories

#### User Story 1:
As a: developer who uses squad tasks on their own machine

I want to:
`awman config set daemon.features.worker.enabled true` while my squad tasks keep running

So I can:
let a tool such as infact push work to the same awman, with the same images, credentials and agent approvals, without a second awman home

#### User Story 2:
As a: operator of a build box

I want to:
`awman config set daemon.features.worker.enabled true`, link a work server, and `awman daemon start --background`, with no network listener at all

So I can:
dedicate the box to pulling work from a work server, with no port open

#### User Story 3:
As a: developer

I want to:
`awman daemon status` to show which features are on (and whether persistently or ephemerally), what each is running, and how much of the shared capacity each uses

So I can:
see one picture of everything awman is doing on this machine

### Implementation Details:
- **Command surface.** It is minimal: the `awman daemon` group has exactly five verbs (catalogue group `daemon`), with no per-action subcommands and no config verb of its own.
  - **Naming:** the CLI group, the process and the config section are all `daemon` (`awman daemon …`, "the awman daemon", `daemon.*`). `engine` is only the name of one feature (0142), turned on with `awman config set daemon.features.engine.enabled true`.
  - `awman daemon start [--background]` starts the daemon with the features enabled in config. It refuses to start with none.
  - `awman daemon stop [--now]` drains by default; `--now` cancels running work.
  - `awman daemon status [--json]` shows:
    - features (`on`, `off`, `ephemeral`, `draining`);
    - listeners;
    - running and queued work per feature;
    - capacity shares.

    It reads `GET /v1/status`.
  - `awman daemon logs [-f]`.
  - `awman daemon api …` is 0139 §6's client-side table of friendly actions plus `--raw`. This item contributes `features.list|get`, `keys.list|create|delete`, `grants.list|delete` and `jobs.cancel` (`feature=` or `id=`). Other items contribute their own actions, each of which maps to one route.
- **Config goes through `awman config`** (0139 §4). It is the one config writer, and there is no `awman daemon set`.
  - All `daemon.*` keys are `global` scope, so `awman config set daemon.…` always writes the global file.
  - Keys marked *live* in 0139's key table are applied to a running daemon at once: `config set` calls `POST /v1/config/reload` over the local listener and prints `applied to the running daemon`.
  - Live keys:
    - every `daemon.features.*` key;
    - `daemon.maxConcurrentJobs`;
    - `daemon.terminal.remoteInput`;
    - `daemon.network.enabled`, `daemon.network.features` and `daemon.network.tlsSans`.
  - `daemon.network.listen` needs a restart, and `config set` says so.
- **Config** (`~/.awman/config.json`, global only; the shape and names are from 0139 §2):
  ```json
  "daemon": {
    "maxConcurrentJobs": 4,
    "terminal": { "cols": 120, "rows": 40, "remoteInput": false },
    "network": { "enabled": false, "listen": "127.0.0.1:9876", "tlsSans": [], "features": ["worker"] },
    "features": {
      "squad":  { "enabled": false, "interaction": "interactive", "maxConcurrentJobs": 1,
                  "maxConcurrentEvaluations": 2, "envPersistence": "keychain" },
      "worker": { "enabled": false, "maxConcurrentJobs": 2,
                  "push": { "enabled": true, "interaction": "headless", "allowOverride": true,
                            "workDirs": { "allow": [], "roots": [] } },
                  "pull": { "enabled": true, "interaction": "interactive", "allowOverride": true, "workRoot": null } },
      "engine": { "enabled": false, "allowEphemeral": true, "idleTimeout": "15m", "maxConcurrentJobs": 4 }
    }
  }
  ```
  - `interaction` and `daemon.terminal` are specified in 0141, `daemon.network.*` in 0140, and `worker.pull` with the top-level `workServers` map in 0143.
  - Squad evaluations take their agents, models, leader and guidance from `workflows.dynamic.*`, and per-task `config.json` files override those (0139 §2). The old `squad.agentsToModels`, `squad.defaultLeader` and `squad.guidance` are gone.
  - `daemon.maxConcurrentJobs` is the host-wide cap. Each feature's `maxConcurrentJobs` is its share.
  - **No migration.** Old keys (`api.*`, `squad.*`, `workers`) are unknown keys, as 0139 specifies.
- **One process, one guard.** `DaemonGuard` changes from "my pidfile, refuse if the other's is live" to a single daemon identity: one pidfile and one startup lock under `<data>/daemon/`.
- **Shared core.** The daemon builds what both daemons build today exactly once:
  - the SQLite store (`TaskStore` and `SqliteSessionStore` on the same pool);
  - container runtime handles;
  - the credential refresh loop;
  - the event bus;
  - the shared terminal-hub interface, implemented for all features and network viewers in 0141;
  - the current agent status behavior; 0141 adds OSC reports later.

  `DaemonFeature` (`src/engine/daemon/feature.rs`) is a Layer 1 lifecycle primitive with `start(&Core)`, `drain()`, `stop_now()` and `status()`; it has no HTTP or command types. Layer 2's `DaemonRuntime` owns feature admission, shared-capacity policy, command construction and typed API operations. Layer 3's API adapter mounts the 0139 route tree over those Layer 2 operations; no `Router`, `axum` handler or request parsing enters Layer 1.
  - **Squad** wraps `SquadDaemonEngine` and `SquadScheduler`.
  - **Worker push** wraps today's API router and `spawn_workers`. **Worker pull** is 0143's claim loop.
  - **Engine** is 0142.
- **One executor.** Work from every feature goes through one executor capped by `daemon.maxConcurrentJobs`. It claims round-robin across features, within each feature's `maxConcurrentJobs`, so a flood of pushed jobs cannot starve squad runs or engine sessions. Within a feature, ordering is unchanged:
  - FIFO per API session for push;
  - the scheduler's order for squad;
  - claim order for pull.
- **One API, two listeners.** One route tree (0139 §5) and one handler stack:
  - **Local listener**, always on while the daemon runs.
    - It is a Unix socket at `<data>/daemon/api.sock` (0600, with a peer-UID check through `SO_PEERCRED`/`getpeereid`), or a named pipe on Windows.
    - The Unix socket uses a private directory, mode 0600 and a peer-UID check; the Windows named pipe uses an owner-only ACL and caller identity check. No loopback fallback or local bearer credential is created.
    - `<data>/daemon/server.json` is an owner-private endpoint metadata sidecar, not a credential store. It records the transport address and build/protocol identity for discovery; it never contains an owner key or other secret.
    - Local CLI, TUI and engine frontends use this listener.
  - **Network listener**, optional and off by default. It is TLS with **API keys** (0140: listen address, SANs, keys).
    - It serves only the features listed in `daemon.network.features` (default `["worker"]`). Any other feature's routes return `404 not_found` there.
    - It **never** serves a feature that was enabled ephemerally.
    - It never serves the local-only core routes.
- **Routes this item owns** (core; 0139 §5 has the full tree):

  | Route | Notes |
  |---|---|
  | `GET /v1/status` | Build, protocol versions, append-only `capabilities: [string]` (0139), features and state, capacity (`max_concurrent_jobs`, running and queued per feature), listeners. |
  | `GET /v1/health` | Unauthenticated, from 0140. |
  | `GET /v1/features`, `GET /v1/features/{feature}`, `PATCH /v1/features/{feature}` | `PATCH {enabled, ephemeral: true}` is the ephemeral switch, local listener only. A persistent change goes through config and `POST /v1/config/reload`. |
  | `GET /v1/config`, `POST /v1/config/reload` | Effective daemon config, with sensitive values masked. Local listener only. |
  | `GET /v1/keys`, `POST /v1/keys`, `DELETE /v1/keys/{id}` | Network API keys (0140). Owner key only. |
  | `GET /v1/grants`, `DELETE /v1/grants/{id}` | Prerequisite grants (was `awman config prerequisite-grants`). |

  The feature prefixes are owned by their items:
  - `/v1/squad/*`: this item, as resources replacing the squad gateway (0139 §5);
  - `/v1/worker/*`: this item moves today's pushed API paths; 0143 adds pull jobs, the unified job model and its routes;
  - `/v1/engine/*`: 0142;
  - `/v1/terminals*`: 0141 implements the shared terminal hub for all features and viewers.

  There are no `/v1/daemon/*` routes and no server-side action routes.
- **Disabled features.** A route of a disabled feature answers `503 {"error": "…", "code": "feature_disabled"}`, so clients can tell "off" from "missing".
- **Squad as resources.** The squad gateway is deleted. Today it forwards CLI command lines (`POST /v1/commands` on the squad daemon, `RemoteTaskGateway`). In its place:
  - `awman squad add|edit|show|list|remove|pause|resume|trigger|cancel|env` call `/v1/squad/tasks*`, `/v1/squad/runs/{id}/cancel` and `/v1/squad/env`;
  - `awman squad attach` keeps its existing local attach path at this milestone; 0141 moves it to the shared `awman attach` resolver.
- **Runtime switching.**
  - Turning a feature on (`awman config set daemon.features.<name>.enabled true`) starts it on the running core.
  - Turning it off stops the feature taking new work, and running work finishes:
    - push submissions get `503 feature_disabled`;
    - squad stops scheduling;
    - pull stops claiming;
    - engine sends its frontends `Detach{reason}` (0142).
  - To cancel running work at once, use `awman daemon api jobs.cancel feature=<name>`.
  - Running work in the other features is untouched.
- **Ephemeral enablement.** A feature can be enabled two ways:
  - **persistently**, through config;
  - **ephemerally**, when a local client needs it. This is `PATCH /v1/features/{feature} {enabled: true, ephemeral: true}`, accepted on the local listener only.

  The first user is 0142's hand-off, which enables `engine` when a TUI starts in Rex (0154).
  - An ephemeral feature is never served on the network listener.
  - An ephemeral `engine` disables itself after `daemon.features.engine.idleTimeout` with no frontends, instances or running work (0142 defines what counts, including 0155's presenter hold). If no feature is left, the daemon exits.
  - `daemon.features.engine.allowEphemeral: false` forbids it.
  - `daemon status` shows `engine: ephemeral (<reason>)`.
- **Supervisor.** One systemd user unit (`awman-daemon`) and one launchd label (`io.awman.daemon`). The unit is started with `AWMAN_HOME` only (0139 §3), so the daemon resolves the same home as the process that started it.
- **Remote.** `awman remote …` targets a remote daemon's `/v1/worker/*` routes, using the target chosen with `--remote <target>` from `remote.targets` (0139).
- **Catalogue.** `FrontendKind::Daemon { feature, listener: Local | Network }` replaces `SquadDaemon` and `Api`. A command's frontend records which feature and listener it came from, so `is_allowed_for_frontend` can apply per-feature and per-listener rules: squad task resources for squad, and `api_allowed` for worker push on the network listener. The match has no default arm, so every command declares its rules.
- **Coordination with 0129.** Land 0129's A01–A03 (API read models, lifecycle out of routes) before this item starts, because both change the same API and daemon code. If this item has already started, the integrator re-sequences with its owner.

### Edge Case Considerations:
- **Disabling the only enabled feature:** the daemon stays up with no features and says so in `status`, unless the last feature was ephemeral, in which case it exits. `awman daemon stop` ends it.
- **Network listener on, and the port in `daemon.network.listen` is taken:** the network listener fails with the bind error and `status` shows it. Features stay on for the local listener, and nothing else is affected.
- **`awman config set` with no daemon running:** the value is persisted and the command says `takes effect on next start`.
- **`awman config set` of a key that isn't live** (`daemon.network.listen`): it is persisted, and the command says `restart the daemon to apply`. It is never half-applied.
- **A crash in one feature's task must not take down the others.** Each feature runs under its own supervisor task that restarts it with backoff, and reports `degraded` in `status`.
- **Squad, push and engine work editing the same records concurrently:** all of it goes through the one store and is serialised by SQLite. Nothing else is needed.
- **An ephemeral `engine` while `engine` is also listed in `network.features`:** it is not served on the network listener until it is enabled persistently.
- **The local listener's socket path already exists** (a stale file from a crash): remove it only while holding the startup lock and after confirming no live pidfile.
- **Windows:** the named pipe ACL and caller identity check allow only the daemon's user. If the local pipe cannot be created or secured, startup fails closed with an actionable error.

### Test Considerations:
- The daemon with `squad` and `worker` both enabled runs a squad trigger and a pushed command concurrently on one database, with no "cannot run at the same time" error.
- `awman config set daemon.features.worker.enabled false` against a running daemon drains: an in-flight command finishes and a new submission gets `503 feature_disabled`. Setting it back to `true` serves again without a restart. `awman daemon api jobs.cancel feature=worker` cancels running work.
- `awman config set` on daemon keys:
  - a live key is applied through `POST /v1/config/reload` and reported as applied;
  - a non-live key reports that a restart is needed;
  - with no daemon running, it persists only and says so;
  - an invalid value is refused by the key table and nothing is persisted.
- `awman daemon api`:
  - `--list` prints every action with its arguments and route;
  - unknown actions, unknown keys and mistyped values are refused locally;
  - each action's request equals its documented `--raw` equivalent;
  - `keys.*` manage network API keys.
- `awman daemon api --raw`:
  - GET and POST to core and feature routes over the local listener print the JSON response;
  - a 4xx or 5xx exits nonzero with the body on stderr;
  - `--data @file` and `--data -` read the body;
  - it is refused with no daemon running.
- Feature routing:
  - a disabled feature gives `503 feature_disabled`;
  - a feature not in `network.features` gives `404` on the network listener;
  - an ephemeral feature is never served on the network listener;
  - local-only core routes (`/v1/features` PATCH, `/v1/config*`) are refused on the network listener;
  - core routes are always present.
- Ephemeral `engine`: it idles out on schedule, the daemon exits when no feature is left, and `allowEphemeral: false` refuses it.
- Local listener security: wrong peer identities are refused on Unix and Windows; a missing, stale or tampered `server.json` cannot grant access, and no local TCP listener appears.
- `daemon.maxConcurrentJobs` is shared, and the round-robin claim lets each feature make progress under load within its `maxConcurrentJobs`.
- Today's API (now `/v1/worker/*`) and squad e2e suites pass against the new daemon, with their paths and lifecycle steps rewritten to the 0139 surface.
- A config with old `api.*`, `squad.*` or `workers` keys fails validation as unknown keys, with the moved-key hint.

### Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: process supervision is Layer 1, startup/admission/key policy is Layer 2, HTTP/socket presentation is Layer 3, and serializable state is Layer 0. The same typed operations back local and network listeners with distinct authorization policy.
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Daemon:** `src/engine/daemon/{guard,supervisor,feature}.rs`, and `src/command/commands/{api_server,squad/daemon,squad/daemon_runtime}.rs`.
- **Router:** `src/frontend/{api,squad}/` merge into one transport adapter with core and per-feature routes backed by Layer 2's typed `DaemonApi` operations. The squad gateway (`RemoteTaskGateway`) is deleted. Router construction, WebSocket upgrades, peer-credential extraction and HTTP serialization stay in Layer 3; admission, config reload, key management and feature lifecycle decisions stay in Layer 2 or Layer 1 as appropriate.
- **Commands:**
  - `src/command/commands/daemon/` holds the five verbs. `api` is the client-side action table in `src/command/commands/daemon/actions.rs`, which other items extend.
  - `src/command/dispatch/catalogue/{daemon,squad,mod}.rs`: the `api` catalogue group and `squad start|stop|status|logs` are deleted.
- **Config:** `src/data/config/daemon.rs` (0139's section struct), plus rows in the key table `src/data/config/fields.rs`. Layer 0 owns daemon paths, metadata persistence and database state; Layer 1 owns process supervision and shared execution mechanics; Layer 2 owns startup and feature policy.
- **Related:**
  - network listener settings and keys: Part B of this item;
  - 0141: terminal hub and interaction;
  - 0143: worker pull, the unified push/pull job model and queue picker;
  - 0142: the engine feature, frontend protocol and hand-off;
  - 0154: Rex, the first ephemeral-engine client;
  - 0139: naming, config and routes.

### Documentation
- New `docs/16-daemon.md`:
  - features;
  - `awman daemon start|stop|status|logs|api`;
  - turning features on and off with `awman config set daemon.features.<name>.enabled`;
  - ephemeral features;
  - the local and network listeners;
  - status.
- Update `docs/09-api-and-remote-mode.md` (API and remote become the `worker` feature's push side, under `/v1/worker/*`) and `docs/12-squad.md` (squad is a feature). Remove every mention of `awman api start|kill|logs|status|key` and `awman squad start|stop|status|logs`; there are no compatibility spellings to document.
- `docs/07-configuration.md` (the `daemon.*` section of 0139's reference; `api.*`, `squad.*` and `workers` removed) and `docs/14-command-reference.md` (the five `awman daemon` verbs and `awman daemon api --list`; the `api` group and `squad start|stop|status|logs` removed).

## Part: Network listener hosting and key rotation

### Summary:
The API server is shaped for one developer on one machine:

- It binds `127.0.0.1` with no option (`api_server.rs:383`).
- Its certificate covers only `127.0.0.1` and `localhost` (`auth/mod.rs:559-560`), so `::1` and real hostnames fail verification.
- `/v1/status` omits the worker count and the global queue.
- There is exactly one key, and `--refresh-key` replaces it in one step, so rotation means downtime.

Running awman next to a coordination server in docker compose, or as one host among several, needs a tunnel today. After 0140 the API server is the daemon's **network listener**. This item gives that listener a configurable address, verifiable certificates, capacity in `/v1/status`, and named keys.

### User Stories

#### User Story 1:
As a: operator running awman in a container next to its client

I want to:
`awman config set daemon.network.listen 0.0.0.0:9876`, `awman config set daemon.network.tlsSans awman.internal` and `awman config set daemon.network.enabled true`

So I can:
reach it over the compose network with TLS that verifies

#### User Story 2:
As a: client scheduling across several awman hosts

I want to:
read `max_concurrent_jobs`, running and queued counts per feature, and `oldest_queued_age_ms` from `/v1/status`

So I can:
route work to the least-loaded host

#### User Story 3:
As a: operator

I want to:
add a second named key, move the client to it, then delete the first

So I can:
rotate credentials without downtime

### Implementation Details:
- **Where this lives after 0140.** The network listener serves the features in `daemon.network.features` (default `["worker"]`, whose push side is today's API). Its settings are rows in 0139's key table, all `global` scope, set with `awman config set` (0139 §4):

  | Key | Default | Live | Meaning |
  |---|---|---|---|
  | `daemon.network.enabled` | `false` | yes | Open the network listener. |
  | `daemon.network.listen` | `"127.0.0.1:9876"` | no (restart) | `host:port` to listen on. `[::]:9876` and IPv6 literals are accepted. |
  | `daemon.network.tlsSans` | `[]` | yes (regenerates the certificate) | Extra DNS names and IPs for the generated certificate. |
  | `daemon.network.features` | `["worker"]` | yes | Features served on this listener (0140). |

- **No old surface.** There are no start flags and no compatibility spellings:
  - `awman api start --bind/--tls-san`, `--dangerously-skip-tls` and `--refresh-key` are removed;
  - the old `api.*` keys are unknown keys (0139).
- **TLS only.** The network listener is always TLS, and there is no plaintext option. Plaintext loopback use was the only reason for `--dangerously-skip-tls`, and the daemon's local listener (Unix socket or named pipe, 0140) now covers local clients.
- **Listen address.**
  - The effective address is logged and shown in `awman daemon status`.
  - The local listener is unaffected by `listen`.
  - `listen` is not live: `awman config set daemon.network.listen …` persists the value and says `restart the daemon to apply`, as 0139 specifies. Toggling `daemon.network.enabled` *is* live: it opens or closes the network listener without touching the local listener or running work.
- **Certificate names.** `tlsSans` entries are added to the generated certificate's SANs, and `::1` is always included. Changing the SANs regenerates the certificate, reloads it into the running listener, and says so.
- **`/v1/status` and `/v1/health`** are core daemon routes (0140, 0139 §5), present whatever features are on. `capabilities` advertises implemented API contracts independently of daemon feature state; `features` remains the per-feature state map.
  - `/v1/status` includes:
    - `version`;
    - `capabilities` (append-only contract flags);
    - `features` (per feature: `state`, `max_concurrent_jobs`, `running`, `queued`, `oldest_queued_age_ms`);
    - the host-wide `max_concurrent_jobs`;
    - `agents` (installed agent names);
    - `listeners` (local, and network with its `listen` address and served features).
  - An unauthenticated `GET /v1/health` returns only `{status}`, for load balancers.
- Add `daemon_features`, `network_listener`, and `api_keys` to `capabilities` when their respective routes and fields ship. Enabling or disabling a feature changes its state in `features`, never the build's capability list.
- **Keys** are credentials for the optional network listener only. They are the core resource `/v1/keys` (0139 §5), callable only by the owner through the authenticated local socket or named pipe:
  - `POST /v1/keys {name}` → `{id: "key_…", name, key, created_at}`, with `key` shown only in this response;
  - `GET /v1/keys` → `{items: [{id, name, created_at, last_used_at}], next_cursor}`, never key material;
  - `DELETE /v1/keys/{id}`.

  The friendly actions are `awman daemon api keys.create name=<name>`, `keys.list` and `keys.delete id=<id>` (0139 §6). Hashes are stored in `<data>/daemon/api_keys.json` (0600). There is no `api key` command group and no `--refresh-key`. "Rotate everything" is `keys.create` followed by `keys.delete` for each old key.
- **Trace propagation.** The push routes `POST /v1/worker/sessions`, `POST /v1/worker/commands` and `POST /v1/worker/runs` accept a `traceparent` header. It is stored on the row and included in the queue status event, which lets clients correlate their traces with awman logs.

### Edge Case Considerations:
- Listening on a non-loopback address inside a container is the common case. Document that the Docker socket mount is the real trust boundary.
- `daemon.network.features` must never expose an ephemerally enabled feature (0140), however it is configured. Listing `engine` with a non-loopback `listen` logs a warning at startup.
- A listen failure fails only the network listener. The local listener and the features keep running, and `awman daemon status` shows the error with the address it tried. The persisted value stays, so the user can correct it and restart.
- Deleting the last key leaves the network listener up but rejecting every request, and `awman daemon status` says so.
- `tlsSans` entries that are neither a valid DNS name nor an IP literal are refused by the key table, and nothing is persisted.

### Test Considerations:
- `daemon.network.listen` parsing (IPv4, IPv6, `[::]`, a missing port refused). Setting it reports that a restart is needed, and a restart listens on the new address.
- `daemon.network.enabled` toggles the listener live, and the local listener stays up throughout.
- SANs in the generated certificate, and live regeneration on change.
- `/v1/status` fields and their 0139 naming (`_ms` durations, per-feature objects).
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

### Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, especially `aspec/architecture/security.md`.
- **Config:** the `daemon.network.*` keys are rows in `src/data/config/fields.rs` within the daemon section struct (`src/data/config/daemon.rs`, 0139). The network listener (`src/frontend/api/serve.rs`) applies them.
- **Routes and commands:**
  - `/v1/keys` is a core route in the merged router (0140);
  - the `keys.*` entries go in the client-side action table, `src/command/commands/daemon/actions.rs` (0139 §6);
  - remove the `api` catalogue group (`src/command/dispatch/catalogue/api.rs`).

### Documentation
- `docs/16-daemon.md`: the network listener, with `daemon.network.*` set through `awman config set`, keys through `/v1/keys` and `awman daemon api keys.*`, and TLS-only.
- `docs/09-api-and-remote-mode.md`: the same, from the client's side. Remove `awman api start` flags, `api key` and `--refresh-key`. Also correct the docs' session discriminator to `session_type`, and show that a command's status comes from `GET /v1/worker/commands/{id}` (the `/status` alias is gone, 0139).
