# The daemon

The awman daemon is one long-running process for the features you enable. It
provides an owner-only local listener for the CLI and TUI, shares execution
capacity between features, and can serve selected features through named
network listeners configured under `daemon.network.listeners`.

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
| `daemon.features.engine.maxConcurrentJobs` | `8` | Engine jobs run at once. |

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

Status groups the process summary, features, and listeners into separate
sections. The feature table aligns state, running, and queued columns;
indented lines show instance counts, worker push/pull summaries, and diagnostics.
Each feature is `on`, `off`, `ephemeral`, `draining`, or `degraded`.
It includes the oldest queued age when
available, restart/error details for a degraded feature, engine idle-out time,
shared capacity, and local/network listener state. Each network listener is
listed by name, address, transport, selected features and any bind error.
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

Each named network listener has its own address, transport, feature list,
browser-origin allowlist and TLS certificate names. The built-in `default`
listener is disabled and serves `worker` over TLS at `127.0.0.1:9876` when
enabled. The built-in `loopback` listener is disabled and intended for
same-machine browser access: it uses `loopback_http` at `127.0.0.1:9877`,
serves `engine`, and allows the hosted web app's origin. Custom listeners start
disabled with no features or allowed origins and need an explicit `listen`
address. A listener exposes only the features listed for it. The feature
choices are `worker`, `squad` and `engine`; `engine` is permitted only on a
loopback address.

The `enabled`, `listen`, `transport`, `features`, `tlsSans` and `allowedOrigins`
fields belong under `daemon.network.listeners.<name>`. `listen` is a
`host:port` address (bracket IPv6 literals); changing it requires a daemon
restart. The other settings reload live. Changing a bound listener's
`transport` rebinds only that listener; its connected clients are disconnected,
and `awman config set` reports how many. Other listeners and the local
socket are unaffected. `transport` is `tls` or `loopback_http`. Plain HTTP is accepted only for the literal loopback addresses
`127.0.0.1` and `[::1]`; `tlsSans` is available only with TLS. TLS certificates
include the listener IP unless it is unspecified, plus configured `tlsSans`
DNS names or IP addresses. TLS listeners use named API keys created through
`awman daemon api keys.create` for ordinary network requests; without a key,
those requests are refused. Browser links are available on the loopback engine
listener when its `allowedOrigins` includes the requesting browser's origin.

For example, enable the default worker listener with:

```sh
awman config set daemon.network.listeners.default.enabled true
awman config set daemon.network.listeners.default.listen 0.0.0.0:9876
awman config set daemon.network.listeners.default.features worker
awman config set daemon.network.listeners.default.tlsSans api.example.org
```

`allowedOrigins` is a list of exact browser origins, including scheme and
port, for example `https://awman.prettysmart.dev` or
`http://127.0.0.1:3000`. It controls which browser origins may connect; it is
not itself authentication. Browser linking requires an allowed origin and an
owner's approval on the local daemon. The `loopback` listener allows the hosted
web app and both local development origins by default. If `allowedOrigins` has
been overridden, include both local origins explicitly:

```sh
awman config set daemon.network.listeners.loopback.allowedOrigins https://awman.prettysmart.dev,http://127.0.0.1:3000,http://localhost:3000
```

Origins must match exactly.
Review and revoke browser grants through the local daemon API with
`awman daemon api browsers.list` and `awman daemon api browsers.delete
id=<browser-id>`. See the [Web App guide](20-webapp.md) for setup.

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
