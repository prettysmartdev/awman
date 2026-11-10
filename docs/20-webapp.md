# Web App

The awman web app is a browser interface to an awman daemon on the same
computer. The hosted app is at <https://awman.prettysmart.dev>. It is a static
site; it does not run agents or proxy browser requests to your daemon.

## Install and connect

Open the hosted app in a browser on the computer running awman. The first visit
requires an internet connection. Use the browser's **Install** or **Add to Home
Screen** command to install it. The installed app can open its cached shell
offline after the first online visit and service-worker installation.

Browser access requires awman and its daemon on the same computer, a browser
that can access loopback addresses, and the daemon's engine feature and built-in
`loopback` listener enabled. In a terminal, configure those two settings and
start the daemon in the background:

```sh
awman config set daemon.features.engine.enabled true
awman config set daemon.network.listeners.loopback.enabled true
awman daemon start --background
```

The app connects to `http://127.0.0.1:9877`. If your browser asks to allow
local-network access, allow it for the app. Once connected, choose **Connect
browser**. The app displays a short `web-<code>` code. Approve it in an
interactive terminal on the daemon's computer:

```sh
awman link web-<code>
```

Review the requesting origin and approve the request at the CLI prompt. A
browser grant authorizes that browser for the engine on that listener. Grants
last twelve hours and are dropped when the daemon stops; approve the browser
again after a daemon restart.

## Workspaces

A browser has no shell of its own, so the web app opens sessions in
**workspaces** you register from a terminal. Run this in the project directory
you want to work in, in the shell whose `PATH` and credentials the agents should
use:

```sh
awman daemon api launch_contexts.create default=true
```

The web app lists registered workspaces and preselects the default. Agents
started from the browser use the registered directory and that shell's
environment; the browser cannot supply its own environment, executable paths
or mount roots. New tabs may only open directories inside a registered
workspace. If the daemon restarts, a workspace shows as **environment stale**
until you refresh it from the same directory:

```sh
awman daemon api launch_contexts.list
awman daemon api launch_contexts.refresh id=<workspace-id>
awman daemon api launch_contexts.delete id=<workspace-id>
```

## Closing or reloading the browser tab

A browser session is a primary frontend of its engine instance, like a TUI.
Reloading the tab reconnects to the same instance when the reload finishes
within 60 seconds. After that, the engine applies its normal exit policy, as if
the TUI had been closed. When a command or workflow is still running, the
browser asks for confirmation before the tab closes or reloads. Another tab on
the same machine can attach to an existing browser instance as a read-only
view, or open its own instance.

The v0 web app cannot connect to a daemon on another machine. A phone or second
computer cannot use a desktop daemon through this web app, even when both are
on the same network.

## Shortcuts

The app follows TUI actions where browsers allow them. Browser-reserved
shortcuts use alternatives or visible buttons: **Ctrl+T**, **Ctrl+W**, and
**Ctrl+N** have **Alt+T**, **Alt+W**, and **Alt+N** alternatives; **Ctrl+Shift+C**
is available as a button. When a text input has focus, the browser keeps
**Ctrl+A** and **Ctrl+D** for editing; use **Alt+Left** and **Alt+Right** to
switch tabs. The in-app shortcut list shows available alternatives.

## Offline behaviour

After the first successful online visit and service-worker installation, the
app shell can open without the hosted site. The local daemon must still be
reachable to use the app. The offline cache contains the static app shell; it
does not cache daemon responses, terminal output, or credentials. A first visit
or install requires internet access.

## Usage reporting and privacy

The hosted site's usage counters record successful page loads and estimate
daily and monthly distinct IP addresses. These are estimates of IP addresses,
not people: one person may use more than one address, and an address may be
shared. The app does not retain raw IP addresses or per-visitor records.
Cloudflare still processes request metadata to serve the hosted site.

## Revoke browser access

List active browser grants from the daemon's computer:

```sh
awman daemon api browsers.list
```

Revoke a grant using the ID shown in the list:

```sh
awman daemon api browsers.delete id=<browser-id>
```

Revocation closes that grant's active browser connections.

## Troubleshooting

Check the daemon and listener state:

```sh
awman daemon status
```

Status lists listener names, addresses, transports, features, allowed origins,
browser grants, and bind errors. The `loopback` listener should be enabled at
`127.0.0.1:9877` and serve `engine`. If it is missing or has a bind error,
check the two settings above and whether another process is using that port.
The hosted app origin must be in `allowedOrigins`; the built-in `loopback`
listener includes it by default. For local development, add the development
origin explicitly. Origins must match exactly, including scheme and port:

```sh
awman config set daemon.network.listeners.loopback.allowedOrigins https://awman.prettysmart.dev,http://localhost:3000
```

`http://127.0.0.1:3000` is a different origin and must be added separately if
needed. Setting `allowedOrigins` replaces the list, so keep the hosted origin
when adding development origins. Use the browser's local-network permission
for the hosted app when it is requested, and recheck after changing listener
settings.
