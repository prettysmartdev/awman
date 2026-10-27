# Work Item: Feature

Title: One awman daemon with switchable squad, API and worker modes
Issue: issuelink

## Summary:
- awman has two long-lived processes, the squad daemon (`awman squad start`) and the API server (`awman api start`), and they refuse to run together. `DaemonGuard` (`src/engine/daemon/guard.rs`) exists so that only one process ever holds `~/.awman/data/awman.db`; its error reads "awman api and the squad daemon cannot run at the same time". So a machine can automate with squad tasks *or* serve API clients, never both. Clients work around it with a second awman home (infact's `make dev` does exactly that).
- Replace both with **one daemon process** that owns the database and runs any combination of three **modes**:
  - **squad**: the squad scheduler, its tasks and evaluations (today's squad daemon);
  - **api**: the HTTP API for sessions and commands (today's API server);
  - **worker** (new, proposal 0147): pulls work items from one or more *work servers* and runs them.
- Modes are switched on and off in config or at runtime, without restarting the daemon or the work running under the other modes.
- `awman api start` and `awman squad start` keep working as compatibility spellings of "start the daemon with this mode enabled".

## User Stories

### User Story 1:
As a: developer who uses squad tasks on their own machine

I want to:
`awman daemon enable api` while my squad tasks keep running

So I can:
let a tool such as infact submit work to the same awman, with the same images, credentials and agent approvals, without a second awman home

### User Story 2:
As a: operator of a build box

I want to:
`awman daemon start --worker --background` and nothing else

So I can:
dedicate the box to pulling work from a work server, with no API port open at all

### User Story 3:
As a: developer

I want to:
`awman daemon status` to show which modes are on, what each is running and how much of the shared capacity each uses

So I can:
see one picture of everything awman is doing on this machine

## Implementation Details:
- **Command tree** (catalogue `daemon`): `awman daemon start [--squad] [--api] [--worker] [--background] [--port N]`, `daemon stop [--now]`, `daemon status [--json]`, `daemon logs`, `daemon enable <mode>`, `daemon disable <mode> [--now]`. With no mode flags, `start` enables the modes listed in config, and refuses to start with none.
- **Config** (`~/.awman/config.json`), all optional:
  ```json
  "daemon": {
    "modes": {
      "squad":  { "enabled": true,  "interaction": "interactive" },
      "api":    { "enabled": false, "interaction": "non-interactive", "port": 9876, "bind": "127.0.0.1" },
      "worker": { "enabled": false, "interaction": "interactive" }
    },
    "maxConcurrent": { "squad": 1, "api": 2, "worker": 2 }
  }
  ```
  `interaction` is specified in 0146 and worker settings in 0147. `bind` is the API listener address that 0141 makes configurable. The existing `workers` key stays the host-wide cap, and `maxConcurrent` gives each mode a share of it.
- **One process, one guard.** `DaemonGuard` changes from "my pidfile, refuse if the other's is live" to a single daemon identity: one pidfile and one startup lock under `<data>/daemon/`. On start it detects a live legacy squad or API pidfile and refuses with a message naming that process. It never takes a legacy process over silently.
- **Shared core.** The daemon builds what both daemons build today exactly once: the SQLite store (`TaskStore` and `SqliteSessionStore` on the same pool), container runtime handles, the credential refresh loop, the event bus and the attach hub (0146). Each mode is a `DaemonMode` with `start(&Core)`, `drain()` and `status()`. Squad wraps `SquadDaemonEngine` and `SquadScheduler`. API wraps today's router and `spawn_workers`. Worker is 0147.
- **One executor.** Commands from every mode go through one executor with the host-wide `workers` cap. It claims round-robin across modes, so a flood of API commands cannot starve squad runs. Within a mode, ordering is unchanged: FIFO per API session, the scheduler's order for squad, and claim order for worker.
- **Control plane.** The local control listener, which is the squad daemon's loopback HTTP and bearer key today, becomes the daemon's control listener, published through the same server-meta sidecar. It serves `daemon enable|disable|status` and the squad gateway (`RemoteTaskGateway`). The API listener (TLS, port 9876, API key) is opened only while api mode is enabled. The keys stay separate: the squad key controls the daemon, and the API key is for API clients.
- **Runtime switching.** `enable` starts a mode on the running core. `disable` stops the mode taking new work (API submissions get 503 `mode_disabled`, squad stops scheduling, workers stop claiming) and lets running work finish. With `--now` it cancels running work. Running work from the other modes is untouched.
- **Supervisor.** One systemd user unit (`awman-daemon`) and one launchd label (`io.awman.daemon`). `squad start --background` and `api start --background` install or enable that unit with their mode added.
- **Compatibility.**
  - `awman api start [flags]` starts the daemon with api enabled, or enables api on a running daemon. It warns once that `awman daemon` is the new spelling.
  - `awman squad start` does the same for squad.
  - `api kill` and `squad stop` disable their mode, and stop the daemon if no mode is left.
  - Squad CLI and TUI entry points that start the daemon on demand call `daemon enable squad` instead.
- **Catalogue.** Add `FrontendKind::Daemon`, which replaces `SquadDaemon`. A command's frontend now records which mode it came from, so `is_allowed_for_frontend` can apply per-mode rules: the squad subtree for the squad gateway, `api_allowed` for the API.

## Edge Case Considerations:
- Disabling the only enabled mode: the daemon stays up with no modes and says so in `status`. `daemon stop` ends it.
- `daemon enable api` when port 9876 is taken: the enable fails with the bind error, and the other modes keep running.
- Upgrades with a legacy daemon running: refuse to start and name it. `daemon start --replace-legacy` stops it gracefully first.
- A crash in one mode's task must not take down the others. Each mode runs under its own supervisor task that restarts it with backoff, and reports `degraded` in `status`.
- Squad and API commands editing the same task concurrently: both now go through the one store and are serialised by SQLite. Nothing else is needed.

## Test Considerations:
- Daemon with squad and api both enabled: a squad trigger and an API command run concurrently on one database, with no "cannot run at the same time" error.
- `disable api` drains: an in-flight command finishes and a new submission gets 503. `enable api` serves again without a restart.
- The `workers` cap is shared, and the round-robin claim lets each mode make progress under load.
- `api start` and `squad start` compatibility paths, including `--background`, and refusal when a legacy pidfile is live.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `src/engine/daemon/{guard,supervisor}.rs`, `src/command/commands/{api_server,squad/daemon,squad/daemon_runtime}.rs`, `src/frontend/{api,squad}/`, `src/command/dispatch/catalogue/{api,squad,mod}.rs`.

## Documentation
- New `docs/16-daemon.md` (modes, config, enable and disable, status). Update `docs/09-api-and-remote-mode.md` and `docs/12-squad.md` to describe them as modes, and `docs/07-configuration.md` (`daemon.*`).
