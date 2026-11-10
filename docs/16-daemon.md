# The daemon

The awman daemon is one long-running process for the features you enable. It
provides an owner-only local listener for the CLI and TUI, shares execution
capacity between features, and can serve worker requests over a network
listener when `daemon.network.enabled` is true.

## Features

The daemon has three features:

- **worker** runs pushed commands and pulled work-server leases. Linked servers
  register with WSP; automatic claims and manual picks share worker capacity.
  Pull management routes are available only on the local listener.
- **squad** evaluates scheduled tasks and runs squad work.
- **engine** hosts TUI sessions (instances) inside the daemon, so a TUI can run as a thin terminal on top of it. The TUI behaves exactly like the normal builtin TUI; see [The Engine and Frontends](19-engine-and-frontends.md).

Features are configured globally. Enable or disable one while the daemon is
running with `awman config set`; these settings apply live:

```sh
awman config set daemon.features.worker.enabled true
awman config set daemon.features.squad.enabled true
awman config set daemon.features.engine.enabled false
```

At least one feature must be enabled when starting the daemon. Disabling the
last persistent feature while it is running leaves the daemon available with
no features until you stop it or enable one again.

The engine feature can also start **ephemerally** when a TUI needs it, when
`daemon.features.engine.allowEphemeral` is enabled. An ephemeral engine is
shown as such in status and shuts down after its idle timeout. When the last
ephemeral feature idles out, the daemon exits. Ephemeral features are local
only and are never exposed through the network listener.

Engine behavior is controlled by four live, global settings:

| Key | Default | Meaning |
|---|---|---|
| `daemon.features.engine.enabled` | `false` | Keep the engine on persistently. |
| `daemon.features.engine.allowEphemeral` | `true` | Let a TUI start the engine on demand. |
| `daemon.features.engine.idleTimeout` | `15m` | How long an unused ephemeral engine stays up. |
| `daemon.features.engine.maxConcurrentJobs` | `4` | Engine jobs run at once. |

A persistent engine stays up until you disable it; an ephemeral one is started on demand and stops once it has been idle for `idleTimeout`. To make it persistent:

```sh
awman config set daemon.features.engine.enabled true
```

List the TUI instances the engine is hosting:

```sh
awman daemon api instances.list
```

See [The Engine and Frontends](19-engine-and-frontends.md) for choosing between the builtin and daemon backends.

## Worker push and pull

The worker feature has two directions. **Push** accepts sessions, commands, and
one-shot runs from clients such as scripts or `awman remote`, using the
`/v1/worker/*` API. **Pull** lets a worker connect to a work server and claim
its queued jobs. The daemon registers linked servers, claims when capacity is
available, and runs accepted leases through the same command dispatcher as push.
Queue and claim return `503 pull_disabled` while pull is disabled or draining.
Network API access exposes worker push only; pull routes are local-only.

Link a server and manage its configuration with the commands described in
[Workers and Work Servers](17-workers-and-work-servers.md#link-a-work-server).
That guide also documents the `workServers.*` settings, labels, claim modes,
and WSP v1 for server authors.

### Worker status

`GET /v1/status` reports the worker feature's usual `state`, `running`, and
`queued` fields, plus push and pull summaries when worker is enabled. The shape
is:

```json
"worker": {
  "push": { "running": 1, "queued": 2 },
  "pull": {
    "servers": [
      { "name": "team", "state": "registered", "running": 1, "queued_eligible": 2 }
    ]
  }
}
```

These are the `push` and `pull` fields inside `features.worker`; the worker
feature record also includes its `name`, `state`, `enabled_by`,
`max_concurrent_jobs`, `running`, `queued`, `oldest_queued_age_ms`,
`restarts`, `last_error`, and `since_at` fields. Each pull server summary has
`name`, `state`, `running`, and `queued_eligible`. Server state can be
`registered`, `registering`, `disconnected`, `revoked`, `error`, `retiring`,
or `disabled`. `awman daemon status` prints push running/queued counts and one
line per pull server.

### Lease management

The local daemon API can list recorded leases and release an active lease. These
are local management actions, even when the daemon's network listener exposes
worker push:

```sh
awman daemon api leases.list
awman daemon api leases.list server=team state=active
awman daemon api leases.release id=lease_0123456789abcdef0123456789abcdef reason=user_declined
```

`leases.list` accepts `server`, `state`, `limit`, and `cursor`; use
`state=active` for held leases. `leases.release` takes a `lease_…` ID and
optional `reason` (`user_declined`, `ineligible`, `shutdown`, or `error`) and
`message`. The default reason is `user_declined`. A release with reason
`shutdown` also pauses claiming for a minute, which is what
`awman daemon stop --now` relies on; a daemon that keeps running resumes
pulling afterwards. The actions map to
`GET /v1/worker/leases` and `POST /v1/worker/leases/{id}/release`.

## Start, stop, status, and logs

Start in the foreground (Ctrl-C stops it):

```sh
awman daemon start
```

To start in the background, use the operating system service manager when
available:

```sh
awman daemon start --background
```

Stop gracefully; running work drains before exit. The daemon waits for active
pulled leases to finish. Use `--now` to release pull leases with reason
`shutdown` before cancelling remaining worker and squad commands:

```sh
awman daemon stop
awman daemon stop --now
```

Inspect feature states, listeners, capacity, queued work, and any degraded
features:

```sh
awman daemon status
awman daemon status --json
```

Status shows each feature as `on`, `off`, `ephemeral`, `draining`, or
`degraded`. It includes running and queued counts, the oldest queued age when
available, restart/error details for a degraded feature, engine idle-out time,
shared capacity, and local/network listener state. If the network listener is
enabled with no named keys, status warns that its requests will be refused.
The status command reports
`awman daemon is not running` when no daemon is available.

Read the daemon log or follow it until Ctrl-C:

```sh
awman daemon logs
awman daemon logs --follow
```

The log records lifecycle events, not agent output:

- features starting, stopping, crashing, and restarting
- engine instances, frontends attaching or detaching, and tabs opening or closing
- commands starting and finishing (with exit code or error)
- agent containers launching and exiting
- workflow runs starting and step transitions
- API sessions, queued commands, and squad runs

Container output stays in each command's own logs. To see more detail, start
the daemon with `RUST_LOG=debug`, which adds lines such as container names and
pending dialogs.

## Local listener and API management

The daemon creates an owner-only Unix socket on Unix systems or a named pipe on
Windows. The CLI and TUI use this local listener; it does not require a network
API key. `awman daemon api --list` lists the supported local actions and their
routes. For example:

```sh
awman daemon api features.list
awman daemon api features.get feature=worker
awman daemon api keys.list
awman daemon api keys.create name=ci
```

A newly created key is printed once. The key list shows names, IDs, creation
and last-use times, never key material. Revoke a key by its ID:

```sh
awman daemon api keys.delete id=key_…
```

Rotate a key by creating a replacement, updating clients to use it, then
deleting the old key. There is no separate refresh operation.

The API client can also send a raw local request with `--raw METHOD /v1/path`
and an optional `--data JSON`, `--data @file`, or `--data -` (stdin). These
management actions use the local listener and are not sent through `awman
remote`.

## Network listener

The network listener is configured independently from the local listener. It
is disabled by default and, when available, serves **TLS only**. It uses named
API keys created through `awman daemon api keys.create`; without keys, network
requests are refused. The network API exposes only the configured
`daemon.network.features` and does not expose owner-only configuration or grant
management routes.

Configure its address, enabled state, feature allowlist, and certificate names
in global config:

```sh
awman config set daemon.network.enabled true
awman config set daemon.network.listen 0.0.0.0:9876
awman config set daemon.network.features worker
awman config set daemon.network.tlsSans api.example.org
```

`daemon.network.listen` is the host and port and requires a daemon restart when
changed. `daemon.network.enabled`, `.features`, and `.tlsSans` are live
settings. The TLS certificate always includes `localhost`, `127.0.0.1`, and
`::1`, plus the listener IP (unless it is unspecified) and configured SANs.
Include any DNS name clients use in `tlsSans`; changing the certificate names
regenerates the certificate, so clients that pinned its previous fingerprint
must pin the replacement.

If the network address cannot be bound, `awman daemon status` reports the address and error. The local listener and enabled features keep running.

### Containers and the Docker socket

When awman runs in a container and the listener binds to a non-loopback
address, the **Docker socket mount is the real trust boundary**. Anyone who can
use that socket can control the host Docker daemon and may gain host-level
control. TLS and API keys protect requests to awman; they do not make an
untrusted Docker socket mount safe. Restrict access to the container and its
socket mount accordingly.

## Configuration reference

The daemon-specific settings live under `daemon.*` in global configuration.
See [Configuration](07-configuration.md#reference-all-keys) for defaults,
environment variable names, and all feature-specific worker and squad settings.

`daemon.terminal.cols` and `daemon.terminal.rows` set the starting dimensions
for daemon-managed interactive terminals (120×40 by default). A changed size
applies to new terminals; an already running terminal retains its size.
`daemon.terminal.remoteInput` defaults to `false`; network viewers can send
input only when this setting and the command's remote-input permission are
both enabled.

The `daemon.features.squad.interaction` and
`daemon.features.worker.push.interaction` settings select `interactive` PTY
mode or `headless` piped mode for each feature. Their defaults are interactive
and headless respectively. Each feature's `allowOverride` (true by default)
controls whether a particular run can request a different mode. The settings
are live and global; denied overrides fail before the run is queued.
`daemon.features.worker.pull.interaction` controls pulled jobs (`interactive` by default), with per-server defaults in `workServers.<name>.interaction`. Refused overrides are released as ineligible before cloning. Linked manual servers register for queue listing; automatic servers claim when shared execution capacity is available. See [Workers and Work Servers](17-workers-and-work-servers.md) for supported execution fields.


## Live changes and shutdown

Disabling a feature applies immediately: new work is refused while accepted jobs drain. Config reload returns without waiting for those jobs. Re-enabling during a drain starts the feature again after its accepted work finishes. Invalid config or a failed feature startup preparation leaves the previous config and active features in place.

`awman daemon stop` stops accepting work and waits, without a time limit, for accepted jobs and pulled leases to finish. Use `--now` to release pulled leases with reason `shutdown` and cancel running work, or send a second shutdown signal to escalate an ongoing drain.

Squad environment persistence is initialized when squad is first enabled. Changing `daemon.features.squad.envPersistence` preserves values in memory. Opting out waits for an active persistence write, prevents queued and future writes to the old backend, and clears its stored item on a best-effort basis.

A feature recovering from a crash reports `degraded`: reads remain available and new work returns `503 feature_degraded` until recovery completes. Network status lists the features actually exposed, excluding ephemeral features.

Worker create routes accept a valid W3C version 00 `traceparent` header. The command record and queue status metadata retain it with the source listener (`local` or `network`); malformed trace headers are ignored.
