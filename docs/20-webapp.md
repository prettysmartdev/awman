# Web App

The awman web app is a browser interface to an awman daemon. The hosted app is
at <https://awman.prettysmart.dev>. It is a static site; it does not run agents
or proxy browser requests to your daemon. Browser requests always go to
`http://127.0.0.1:9877` and `ws://127.0.0.1:9877` on the browser's own computer,
whichever address loaded the page.

## Install and connect

Open the hosted app in a browser. The first visit requires an internet
connection. Use the browser's **Install** or **Add to Home
Screen** command to install it. The installed app can open its cached shell
offline after the first online visit and service-worker installation.

With the current loopback-only engine listener, connecting requires the browser
and daemon on the same computer, plus the daemon's engine feature and built-in
`loopback` listener enabled. In a terminal, configure those two settings and
start the daemon in the background:

```sh
awman config set daemon.features.engine.enabled true
awman config set daemon.network.listeners.loopback.enabled true
awman daemon start --background
```

Whether it is loaded from the hosted site, an installed copy or a local
development address, the app connects only to `127.0.0.1:9877`, where the
built-in daemon listener binds. The page's hostname or scheme never changes
that destination, and the site's content security policy allows no other
daemon address. A phone or another computer cannot reach your daemon through
the web app. If your browser asks to allow
local-network access, allow it for the app. Once connected, choose **Connect
browser**. The app displays a short `web-<code>` code. Approve it in an
interactive terminal on the daemon's computer:

```sh
awman link web-<code>
```

Review the requesting origin and approve the request at the CLI prompt. A
browser grant authorizes that browser for the engine on that listener. Grants
last twelve hours and are dropped when the daemon stops. Within those twelve
hours, reloading the page does not need a new approval; see
[Reloading and relinking](#reloading-and-relinking).

## Run the web app locally

From the repository root, install the web dependencies and start the development
server with `make web/dev`. Open <http://127.0.0.1:3000> or
<http://localhost:3000>. If Tailscale is
running, the target also prints a URL using this machine's tailnet IPv4 address;
a page loaded from that address still connects to `127.0.0.1:9877` on the
browser's own computer, so it only works in a browser on this machine. Both local origins are allowed by the default loopback listener. If you have
overridden `allowedOrigins`, include both local origins in its exact origin list:

```sh
make web/reinstall
awman config set daemon.network.listeners.loopback.allowedOrigins https://awman.prettysmart.dev,http://127.0.0.1:3000,http://localhost:3000
make web/dev
```

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
or mount roots. Manually opened tabs stay inside a registered workspace;
the engine may also open a workspace it prepared for a claimed work item.
If the daemon restarts, a workspace shows as **environment stale**
until you refresh it from the same directory:

```sh
awman daemon api launch_contexts.list
awman daemon api launch_contexts.refresh id=<workspace-id>
awman daemon api launch_contexts.delete id=<workspace-id>
```

## Getting around the workspace

The workspace has one app bar across the top, a context bar below it, the
stage in the middle, and the command box at the bottom.

- **Tabs.** The app bar holds your tabs and a **+** button for a new one. Each
  tab has a close button and a tab menu. On a narrow screen the current tab is
  a single button that opens the list of all tabs, with New tab and Close tab.
- **Squad.** The **Squad** button opens the squad view, or returns to it when
  it is already open. A dot on the button shows squad health. Squad tasks have
  a menu for Edit, Pause/Resume and History; Cancel run and Delete are kept
  apart at the bottom of it.
- **Queue.** The **Queue** button shows a badge when work items are waiting and
  opens the queue beside the workspace (see [Work queue](#work-queue)).
- **Actions.** The **Actions** button opens the command menu: a searchable list
  of everything you can do, grouped as go to, run, agent, workflow, workspace
  and danger actions. Choosing a command from the Run group puts it in the
  command box without running it, so you can add arguments first. Actions that
  are unavailable stay listed, greyed out, with the reason beside them, such
  as read-only view or nothing running.
- **App menu (⋯).** Settings, Keyboard shortcuts, About this app and Quit awman.
- **Context bar.** Shows the workspace directory and worktree, a **Git** chip
  with a summary of changed files, a **Workflow** chip when the tab has a
  workflow, and a connection indicator (for example **Read only** or
  **Reconnecting…**).
- **Stage.** Switch between **Agent** and **Output**. When a tab runs several
  agents, the agent switcher chooses which one you see.
- **Command box.** **Run** starts the command and becomes **Stop** while one is
  running; use the multi-line toggle to write a command over several lines
  without a keyboard shortcut.

Git, Workflow and Queue open in a side panel called the inspector. Only one is
open at a time; opening another replaces it. **Git** lists changed files and
is read-only. **Workflow** shows the steps, with **Control workflow**, a
per-step Control button, Copy context path and **Cancel workflow**. On a wide
window the inspector sits beside the stage; on a narrower one it slides over
the stage as a drawer. Close it with its **Close** button or **Escape**, and
focus returns to the control that opened it.

On a narrow screen (a phone, or a browser zoomed far in), one panel shows at a
time. Choose it from the navigation bar at the bottom: **Agent**, **Output**,
**Workflow**, **Git** or **More**. **More** holds Queue, Squad, Settings,
Keyboard shortcuts and Quit awman. A small dot on a panel means it needs
attention, and the active agent's name and state stay pinned under the app bar
on every panel. The command box stays above the navigation bar. Switching
panels never answers or dismisses a dialog.

Every action is reachable by pointer and by Tab and Enter; keyboard shortcuts
(see [Shortcuts](#shortcuts)) are accelerators for the same actions. Dialogs
open as a centred sheet on a large window and as a bottom sheet on a narrow
one. The app follows your system's light or dark setting, and respects reduced
motion and reduced transparency.

## Reloading and relinking

A browser session is a primary frontend of its engine instance, like a TUI.
Reloading the tab, or closing it and coming back, does **not** normally need
`awman link` again. While the browser's authorization is still valid, the app
shows **Reconnecting this browser using its saved authorization** and returns
you to the same workspace and tab. When it finishes you see **Reconnected to
your workspace**.

Things to know about a reload:

- Reconnecting does not resend anything. A command that was running keeps
  running; commands you typed and dialog answers are not repeated. Check the
  current status before repeating an action.
- The reload must finish within 60 seconds for the workspace to be resumed.
  After that, the engine applies its normal exit policy, as if the TUI had been
  closed, and the app asks you to choose a workspace. Nothing is restarted.
- A reload does not extend the twelve-hour limit. The authorization still ends
  twelve hours after you first approved it.
- Terminals reconnect as view-only. Use **Take control** (or press a key in the
  terminal) to type again; the terminal then resizes to fit.
- When a command or workflow is still running, the browser asks for
  confirmation before the tab closes or reloads.
- Another tab on the same machine can attach to the same workspace as a
  read-only view, or open its own. If a second tab tries to take over a
  workspace that another tab controls, it is told so and can view it, return to
  the other tab, or open a new workspace.
- If several saved authorizations match (for example after linking more than
  once), the app asks which to use. A browser keeps up to eight; when the limit
  is reached it asks you to forget one before linking again.

### When you must link again

You need to run `awman link web-<code>` again, using **Connect browser**, when:

| What happened | What you see |
| --- | --- |
| The twelve hours ended | "This browser's 12-hour authorization has expired." |
| The owner revoked this browser (`awman daemon api browsers.delete`) | "This browser's access was revoked." |
| The daemon restarted (grants are dropped when it stops) | "The daemon has restarted since this browser was linked." Start the daemon, then link again. |
| The daemon is stopping or has stopped | "The daemon is stopping or has stopped." Start it, choose **Recheck**, and link again after it restarts. |
| The listener's address, port or features changed, or the listener was turned off | "The daemon listener has changed…" or "…was disabled or changed…". Fix the listener settings, choose **Recheck**, and link again. |
| This site's origin was removed from `allowedOrigins` | "This site's origin is no longer allowed by the daemon listener." Ask the owner to review the list, **Recheck**, then link again. |
| You opened the app from a different origin than the one you linked from | "This authorization belongs to a different site origin." Return to the original site, or link from here. |
| The browser's saved authorization was cleared (site data cleared, private window, storage unavailable) | "This browser could not recover its saved authorization." |

If the browser cannot store its authorization at all, the app says so and offers
**Connect for this page**; it works until you reload, and then needs another
link approval.

A few situations do **not** need a new link:

- **The daemon did not answer.** The app says "The daemon did not answer" and
  that your saved authorization has not been discarded. Check `awman daemon
  status`, then choose **Recheck**.
- **A short connection drop.** The workspace stays on screen and read-only with
  **Reconnecting…** until it is back.
- **Too many attempts.** The app waits and retries by itself.
- **An older daemon.** If the daemon does not support saved authorization, the
  app asks you to update awman, then **Recheck**. Until then each reload needs a
  new link.

The v0 web app cannot connect to a daemon on another machine. A phone or second
computer cannot use a desktop daemon through this web app, even when both are
on the same network.

## Work queue

Enable the instance worker in the project before opening a browser workspace:

```sh
awman config set tui.worker.enabled true
```

The **Queue** button and **Ctrl+Q** open the queue panel (the inspector) with
eligible items from linked work servers; the workspace stays visible beside or
behind it. Choose an item to view its details, then **Pick** to claim it and run it in a
new tab. When the item belongs to your checkout, choose a worktree or a fresh
clone before the run starts. Closing a tab with an active lease offers
**Release**, **Cancel run**, or **Keep running**. The browser sends these actions
through its engine
instance; the owner-only worker routes remain on the daemon socket. Link work
servers from a terminal with `awman link <server-url>`.

## The agent terminal

Each agent's terminal has one header row: the **Agent** / **Output** switch, the
agent selector (when a tab runs several agents), the agent's status, **Take
control** and an **Agent actions** menu with Send Ctrl+C, Paste, Copy selection,
Back to live, detected links, Focus terminal, Collapse agent and Hide agent.
Menus, command suggestions, notices, a yolo countdown and terminal messages
open over the workspace, so they never change the terminal's size or make the
agent redraw. The Ctrl/Esc/arrow key bar appears only on touch screens.

Switching agents or tabs keeps each terminal connected, with its scrollback and
your control. If the workspace connection drops briefly, the terminal stays
open and only typing pauses until it is back.

The terminal follows its window. When you resize the browser, zoom, change the
font size, open or close the inspector, or switch panels on a narrow screen, the
agent's terminal refits to the space it has and the agent redraws at the new
size. Only the tab that holds control resizes the agent; a read-only view or a
terminal that is waiting for control never changes its size. After a reconnect
the terminal is view-only until you take control, and then it refits. A hidden
or collapsed terminal does not shrink the agent.

The terminal never takes keyboard focus by itself. Click in it, use **Take
control**, or choose **Agent actions → Focus terminal**. Without control,
**Tab** moves on to the next control as usual. While you hold control, every
key, including **Tab**, goes to the agent; press **Ctrl+]** to leave the
terminal (focus moves to its header).

On a short window, such as a phone held sideways or a desktop zoomed to 200%,
the terminal keeps a usable height and the page scrolls to reach the command
box. Information notices disappear after a few seconds; warnings and errors
stay until you dismiss them. When an app update is ready, a banner above the
workspace offers **Reload to update** or **Later**.

## Shortcuts

Shortcuts are accelerators: every action also has a button or a command-menu
entry, and **Actions → Keyboard shortcuts** (or the app menu) lists the keys.
The app follows TUI actions where browsers allow them. Browser-reserved
shortcuts use alternatives or visible buttons: **Ctrl+T**, **Ctrl+W**, and
**Ctrl+N** have **Alt+T**, **Alt+W**, and **Alt+N** alternatives; **Ctrl+Shift+C**
is available as a button. When a text input has focus, the browser keeps
**Ctrl+A** and **Ctrl+D** for editing; use **Alt+Left** and **Alt+Right** to
switch tabs. The in-app shortcut list shows available alternatives.

The **Stop** button in the command box and **Actions → Stop command** always
stop the running command (a workflow asks for confirmation first). **Ctrl+C** copies selected
text when there is a selection and stops otherwise. **Escape** acts on what has
focus: inside the Git or Workflow panel it closes that panel; it cancels a yolo
countdown only from the countdown banner or when nothing else has focus. The
countdown does not block the rest of the workspace, so Actions, Queue, Stop,
and **Cancel workflow** stay available while it runs. In the Workflow panel,
**Shift+Left** and **Shift+Right** select the previous or next step.

A read-only view, or a workspace that is reconnecting, keeps reading controls
such as copying, scrolling, the status log and closing panels; only actions
that change the workspace are disabled, with the reason shown beside them.

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

Configuration changes also end browser access. Removing an origin from
`allowedOrigins` ends the grants for that origin. Disabling or removing a
listener, changing its address, port or transport, or no longer serving
`engine` on it ends every grant on that listener. Each affected browser must
then be linked again. A grant also ends after 12 hours, even if the computer
was asleep or its clock changed in the meantime. In each case the browser shows
why on its next load; see [When you must link again](#when-you-must-link-again).

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
listener includes it and both local development origins by default. If you
overrode `allowedOrigins`, add both development origins explicitly. Origins
must match exactly, including scheme and port:

```sh
awman config set daemon.network.listeners.loopback.allowedOrigins https://awman.prettysmart.dev,http://127.0.0.1:3000,http://localhost:3000
```

Setting `allowedOrigins` replaces the list, so keep the hosted origin
when adding development origins. Use the browser's local-network permission
for the hosted app when it is requested, and recheck after changing listener
settings.
