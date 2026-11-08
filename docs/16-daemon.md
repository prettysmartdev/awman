# The daemon

The awman daemon is one long-running process for the features you enable. It
provides an owner-only local listener for the CLI and TUI, shares execution
capacity between features, and can serve worker requests over a network
listener when `daemon.network.enabled` is true.

## Features

The daemon has three features:

- **worker** accepts pushed commands and runs worker sessions.
- **squad** evaluates scheduled tasks and runs squad work.
- **engine** provides shared TUI engines.

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

Stop gracefully; running work drains before exit. Use `--now` to cancel running
worker and squad jobs first:

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
Configuration also accepts `daemon.features.worker.pull.interaction`
(`interactive` by default), but the daemon does not currently run pulled jobs,
so that setting has no effect yet.


## Live changes and shutdown

Disabling a feature applies immediately: new work is refused while accepted jobs drain. Config reload returns without waiting for those jobs. Re-enabling during a drain starts the feature again after its accepted work finishes. Invalid config or a failed feature startup preparation leaves the previous config and active features in place.

`awman daemon stop` drains without a time limit. Use `--now` to cancel running work, or send a second shutdown signal to escalate an ongoing drain.

Squad environment persistence is initialized when squad is first enabled. Changing `daemon.features.squad.envPersistence` preserves values in memory. Opting out waits for an active persistence write, prevents queued and future writes to the old backend, and clears its stored item on a best-effort basis.

A feature recovering from a crash reports `degraded`: reads remain available and new work returns `503 feature_degraded` until recovery completes. Network status lists the features actually exposed, excluding ephemeral features.

Worker create routes accept a valid W3C version 00 `traceparent` header. The command record and queue status metadata retain it with the source listener (`local` or `network`); malformed trace headers are ignored.
