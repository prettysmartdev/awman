# Work Item: Feature

Title: One awman daemon, one API, with squad, worker and engine features
Issue: issuelink

Depends on: 0159 (naming rules, config tree, route tree).

## Summary:
- Today awman has two long-lived processes, and they refuse to run together:
  - the squad daemon (`awman squad start`);
  - the API server (`awman api start`).
  
  `DaemonGuard` (`src/engine/daemon/guard.rs`) exists so that only one process ever holds `~/.awman/data/awman.db`. Its error reads "awman api and the squad daemon cannot run at the same time". So a machine can automate with squad tasks *or* serve API clients, never both. Clients work around it with a second awman home (infact's `make dev` does exactly that).
- Replace both with **one daemon process** that owns the database and serves **one API**. Its capabilities are **features**, each enabled and disabled independently, in config or at runtime, without restarting the daemon or the work running under the other features:
  - **`squad`**: today's squad functionality, unchanged: the scheduler, tasks, triggers, evaluations and env sync. It becomes a feature of the one daemon instead of its own daemon.
  - **`worker`**: runs jobs that are **pushed** to it or **pulled** by it.
    - Push is today's API server and `awman remote` job submission.
    - Pull is the Work Server Protocol (0147, infact's pull mode).
    - Both share one job model, one executor share and one event stream.
  - **`engine`**: a singleton awman instance hosted in the daemon. It can drive one or more TUI instances, the Rex integration (0151), and any future awman frontend. Its behaviour is specified in 0154 and its frontend protocol in 0155. This item provides the feature slot, the local listener it is served on, and ephemeral enablement.
- There are no modes, and no client-specific or Rex-specific features.
- Every config key, route, ID, list, error, duration and CLI verb in this item follows **0159**, which is the reference for the whole surface.
- awman is in beta, so this is a clean break, with no compatibility spellings, aliases or config migrations:
  - The `awman api …` command group and `awman squad start|stop|status|logs` are removed. Squad's task-management subcommands stay; daemon lifecycle and status live in `awman daemon`.
  - Old `api.*`, `squad.*` and `workers` config keys become unknown keys that fail normal config validation, with 0159's moved-key hint.

## User Stories

### User Story 1:
As a: developer who uses squad tasks on their own machine

I want to:
`awman config set daemon.features.worker.enabled true` while my squad tasks keep running

So I can:
let a tool such as infact push work to the same awman, with the same images, credentials and agent approvals, without a second awman home

### User Story 2:
As a: operator of a build box

I want to:
`awman config set daemon.features.worker.enabled true`, link a work server, and `awman daemon start --background`, with no network listener at all

So I can:
dedicate the box to pulling work from a work server, with no port open

### User Story 3:
As a: developer

I want to:
`awman daemon status` to show which features are on (and whether persistently or ephemerally), what each is running, and how much of the shared capacity each uses

So I can:
see one picture of everything awman is doing on this machine

## Implementation Details:
- **Command surface.** It is minimal: the `awman daemon` group has exactly five verbs (catalogue group `daemon`), with no per-action subcommands and no config verb of its own.
  - **Naming:** the CLI group, the process and the config section are all `daemon` (`awman daemon …`, "the awman daemon", `daemon.*`). `engine` is only the name of one feature (0154), turned on with `awman config set daemon.features.engine.enabled true`.
  - `awman daemon start [--background]` starts the daemon with the features enabled in config. It refuses to start with none.
  - `awman daemon stop [--now]` drains by default; `--now` cancels running work.
  - `awman daemon status [--json]` shows:
    - features (`on`, `off`, `ephemeral`, `draining`);
    - listeners;
    - running and queued work per feature;
    - capacity shares.
    
    It reads `GET /v1/status`.
  - `awman daemon logs [-f]`.
  - `awman daemon api …` is 0159 §6's client-side table of friendly actions plus `--raw`. This item contributes `features.list|get`, `keys.list|create|delete`, `grants.list|delete` and `jobs.cancel` (`feature=` or `id=`). Other items contribute their own actions, each of which maps to one route.
- **Config goes through `awman config`** (0159 §4). It is the one config writer, and there is no `awman daemon set`.
  - All `daemon.*` keys are `global` scope, so `awman config set daemon.…` always writes the global file.
  - Keys marked *live* in 0159's key table are applied to a running daemon at once: `config set` calls `POST /v1/config/reload` over the local listener and prints `applied to the running daemon`.
  - Live keys:
    - every `daemon.features.*` key;
    - `daemon.maxConcurrentJobs`;
    - `daemon.terminal.remoteInput`;
    - `daemon.network.enabled`, `daemon.network.features` and `daemon.network.tlsSans`.
  - `daemon.network.listen` needs a restart, and `config set` says so.
- **Config** (`~/.awman/config.json`, global only; the shape and names are from 0159 §2):
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
  - `interaction` and `daemon.terminal` are specified in 0146, `daemon.network.*` in 0142, and `worker.pull` with the top-level `workServers` map in 0147.
  - Squad evaluations take their agents, models, leader and guidance from `workflows.dynamic.*`, and per-task `config.json` files override those (0159 §2). The old `squad.agentsToModels`, `squad.defaultLeader` and `squad.guidance` are gone.
  - `daemon.maxConcurrentJobs` is the host-wide cap. Each feature's `maxConcurrentJobs` is its share.
  - **No migration.** Old keys (`api.*`, `squad.*`, `workers`) are unknown keys, as 0159 specifies.
- **One process, one guard.** `DaemonGuard` changes from "my pidfile, refuse if the other's is live" to a single daemon identity: one pidfile and one startup lock under `<data>/daemon/`.
- **Shared core.** The daemon builds what both daemons build today exactly once:
  - the SQLite store (`TaskStore` and `SqliteSessionStore` on the same pool);
  - container runtime handles;
  - the credential refresh loop;
  - the event bus;
  - **0146's terminal hub**;
  - **0152's program status**.
  
  Each feature is a `DaemonFeature` (`src/engine/daemon/feature.rs`) with `start(&Core)`, `drain()`, `stop_now()`, `status()` and `routes() -> Router`. The router is mounted at that feature's prefix (`/v1/squad`, `/v1/worker`, `/v1/engine`).
  - **Squad** wraps `SquadDaemonEngine` and `SquadScheduler`.
  - **Worker push** wraps today's API router and `spawn_workers`. **Worker pull** is 0147's claim loop.
  - **Engine** is 0154.
- **One executor.** Work from every feature goes through one executor capped by `daemon.maxConcurrentJobs`. It claims round-robin across features, within each feature's `maxConcurrentJobs`, so a flood of pushed jobs cannot starve squad runs or engine sessions. Within a feature, ordering is unchanged:
  - FIFO per API session for push;
  - the scheduler's order for squad;
  - claim order for pull.
- **One API, two listeners.** One route tree (0159 §5) and one handler stack:
  - **Local listener**, always on while the daemon runs.
    - It is a Unix socket at `<data>/daemon/api.sock` (0600, with a peer-UID check through `SO_PEERCRED`/`getpeereid`), or a named pipe on Windows.
    - A loopback TCP fallback authenticates with the **owner key** from the 0600 server-meta sidecar. This replaces the squad daemon's loopback HTTP and `AWMAN_SQUAD_KEY`.
    - The owner key controls the daemon.
    - Local CLI, TUI and engine frontends use this listener.
  - **Network listener**, optional and off by default. It is TLS with **API keys** (0142: listen address, SANs, keys).
    - It serves only the features listed in `daemon.network.features` (default `["worker"]`). Any other feature's routes return `404 not_found` there.
    - It **never** serves a feature that was enabled ephemerally.
    - It never serves the local-only core routes.
- **Routes this item owns** (core; 0159 §5 has the full tree):

  | Route | Notes |
  |---|---|
  | `GET /v1/status` | Build, protocol versions, features and state, capacity (`max_concurrent_jobs`, running and queued per feature), listeners. |
  | `GET /v1/health` | Unauthenticated, from 0142. |
  | `GET /v1/features`, `GET /v1/features/{feature}`, `PATCH /v1/features/{feature}` | `PATCH {enabled, ephemeral: true}` is the ephemeral switch, local listener only. A persistent change goes through config and `POST /v1/config/reload`. |
  | `GET /v1/config`, `POST /v1/config/reload` | Effective daemon config, with sensitive values masked. Local listener only. |
  | `GET /v1/keys`, `POST /v1/keys`, `DELETE /v1/keys/{id}` | Network API keys (0142). Owner key only. |
  | `GET /v1/grants`, `DELETE /v1/grants/{id}` | Prerequisite grants (was `awman config prerequisite-grants`). |
  
  The feature prefixes are owned by their items:
  - `/v1/squad/*`: this item, as resources replacing the squad gateway (0159 §5);
  - `/v1/worker/*`: 0147;
  - `/v1/engine/*`: 0154 and 0155;
  - `/v1/terminals*`: 0146.
  
  There are no `/v1/daemon/*` routes and no server-side action routes.
- **Disabled features.** A route of a disabled feature answers `503 {"error": "…", "code": "feature_disabled"}`, so clients can tell "off" from "missing".
- **Squad as resources.** The squad gateway is deleted. Today it forwards CLI command lines (`POST /v1/commands` on the squad daemon, `RemoteTaskGateway`). In its place:
  - `awman squad add|edit|show|list|remove|pause|resume|trigger|cancel|env` call `/v1/squad/tasks*`, `/v1/squad/runs/{id}/cancel` and `/v1/squad/env`;
  - `awman squad attach` resolves the run's terminal and calls `awman attach` (0146).
- **Runtime switching.**
  - Turning a feature on (`awman config set daemon.features.<name>.enabled true`) starts it on the running core.
  - Turning it off stops the feature taking new work, and running work finishes:
    - push submissions get `503 feature_disabled`;
    - squad stops scheduling;
    - pull stops claiming;
    - engine sends its frontends `Detach{reason}` (0155).
  - To cancel running work at once, use `awman daemon api jobs.cancel feature=<name>`.
  - Running work in the other features is untouched.
- **Ephemeral enablement.** A feature can be enabled two ways:
  - **persistently**, through config;
  - **ephemerally**, when a local client needs it. This is `PATCH /v1/features/{feature} {enabled: true, ephemeral: true}`, accepted on the local listener only.
  
  The first user is 0155's hand-off, which enables `engine` when a TUI starts in Rex (0151).
  - An ephemeral feature is never served on the network listener.
  - An ephemeral `engine` disables itself after `daemon.features.engine.idleTimeout` with no frontends, instances or running work (0154 defines what counts, including 0157's presenter hold). If no feature is left, the daemon exits.
  - `daemon.features.engine.allowEphemeral: false` forbids it.
  - `daemon status` shows `engine: ephemeral (<reason>)`.
- **Supervisor.** One systemd user unit (`awman-daemon`) and one launchd label (`io.awman.daemon`). The unit is started with `AWMAN_HOME` only (0159 §3), so the daemon resolves the same home as the process that started it.
- **Remote.** `awman remote …` targets a remote daemon's `/v1/worker/*` routes, using the target chosen with `--remote <target>` from `remote.targets` (0159).
- **Catalogue.** `FrontendKind::Daemon { feature, listener: Local | Network }` replaces `SquadDaemon` and `Api`. A command's frontend records which feature and listener it came from, so `is_allowed_for_frontend` can apply per-feature and per-listener rules: squad task resources for squad, and `api_allowed` for worker push on the network listener. The match has no default arm, so every command declares its rules.
- **Coordination with 0129.** Land 0129's A01–A03 (API read models, lifecycle out of routes) before this item starts, because both change the same API and daemon code. If this item has already started, the integrator re-sequences with its owner.

## Edge Case Considerations:
- **Disabling the only enabled feature:** the daemon stays up with no features and says so in `status`, unless the last feature was ephemeral, in which case it exits. `awman daemon stop` ends it.
- **Network listener on, and the port in `daemon.network.listen` is taken:** the network listener fails with the bind error and `status` shows it. Features stay on for the local listener, and nothing else is affected.
- **`awman config set` with no daemon running:** the value is persisted and the command says `takes effect on next start`.
- **`awman config set` of a key that isn't live** (`daemon.network.listen`): it is persisted, and the command says `restart the daemon to apply`. It is never half-applied.
- **A crash in one feature's task must not take down the others.** Each feature runs under its own supervisor task that restarts it with backoff, and reports `degraded` in `status`.
- **Squad, push and engine work editing the same records concurrently:** all of it goes through the one store and is serialised by SQLite. Nothing else is needed.
- **An ephemeral `engine` while `engine` is also listed in `network.features`:** it is not served on the network listener until it is enabled persistently.
- **The local listener's socket path already exists** (a stale file from a crash): remove it only while holding the startup lock and after confirming no live pidfile.
- **Windows:** the named pipe ACL allows only the daemon's user. The loopback fallback is used where named pipes are unavailable.

## Test Considerations:
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
- Local listener security: the wrong peer UID is refused, and a missing or wrong owner key is refused on the loopback fallback.
- `daemon.maxConcurrentJobs` is shared, and the round-robin claim lets each feature make progress under load within its `maxConcurrentJobs`.
- Today's API (now `/v1/worker/*`) and squad e2e suites pass against the new daemon, with their paths and lifecycle steps rewritten to the 0159 surface.
- A config with old `api.*`, `squad.*` or `workers` keys fails validation as unknown keys, with the moved-key hint.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Daemon:** `src/engine/daemon/{guard,supervisor,feature}.rs`, and `src/command/commands/{api_server,squad/daemon,squad/daemon_runtime}.rs`.
- **Router:** `src/frontend/{api,squad}/` merge into one router with core routes plus per-feature sub-routers. The squad gateway (`RemoteTaskGateway`) is deleted.
- **Commands:**
  - `src/command/commands/daemon/` holds the five verbs. `api` is the client-side action table in `src/command/commands/daemon/actions.rs`, which other items extend.
  - `src/command/dispatch/catalogue/{daemon,squad,mod}.rs`: the `api` catalogue group and `squad start|stop|status|logs` are deleted.
- **Config:** `src/data/config/daemon.rs` (0159's section struct), plus rows in the key table `src/data/config/fields.rs`.
- **Related:**
  - 0142: network listener settings and keys;
  - 0146: terminal hub and interaction;
  - 0147: the worker feature;
  - 0154: the engine feature;
  - 0155: the frontend protocol and hand-off;
  - 0151: Rex, the first ephemeral-engine client;
  - 0159: naming, config and routes.

## Documentation
- New `docs/16-daemon.md`:
  - features;
  - `awman daemon start|stop|status|logs|api`;
  - turning features on and off with `awman config set daemon.features.<name>.enabled`;
  - ephemeral features;
  - the local and network listeners;
  - status.
- Update `docs/09-api-and-remote-mode.md` (API and remote become the `worker` feature's push side, under `/v1/worker/*`) and `docs/12-squad.md` (squad is a feature). Remove every mention of `awman api start|kill|logs|status|key` and `awman squad start|stop|status|logs`; there are no compatibility spellings to document.
- `docs/07-configuration.md` (the `daemon.*` section of 0159's reference; `api.*`, `squad.*` and `workers` removed) and `docs/14-command-reference.md` (the five `awman daemon` verbs and `awman daemon api --list`; the `api` group and `squad start|stop|status|logs` removed).
