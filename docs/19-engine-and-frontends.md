# The Engine and Frontends

By default the TUI runs everything inside its own process. awman can instead run the TUI's work on the [daemon's](16-daemon.md) **engine** feature: the TUI becomes a thin terminal on top of an *instance* that lives in the daemon. The TUI looks and behaves exactly the same either way: same tabs, dialogs, workflows, shortcuts and exit behavior. See [Using the TUI](02-using-the-tui.md).

## Choosing where the TUI runs

The `tui.backend` setting picks the backend:

| Value | Behavior |
|-------|----------|
| `auto` (default) | Use the daemon when it is needed (an enabled [integration](#integrations) asks for it) or when the engine feature is configured on; otherwise run builtin. |
| `daemon` | Always run on the daemon engine. If that is impossible, awman prints `engine: unavailable: <reason>` and exits with a nonzero status. |
| `builtin` | Always run in the TUI process. The daemon is never contacted. |

Set it in config, for one invocation, or from the environment:

```sh
awman config set tui.backend daemon
awman --set tui.backend=builtin
AWMAN_TUI_BACKEND=daemon awman
```

Precedence is the usual one (flag, `--set`, environment, repo file, global file, default); see [Configuration](07-configuration.md).

When the daemon is needed but not running, awman starts it in the background. If the daemon is running with the engine off and `daemon.features.engine.allowEphemeral` is true, awman turns the engine on **ephemerally** for you; it stops again after `daemon.features.engine.idleTimeout` with nothing using it. To keep the engine always available, enable it persistently:

```sh
awman config set daemon.features.engine.enabled true
```

See [The daemon](16-daemon.md#features) for the engine settings and `awman daemon status`.

## Instances and frontends

An **instance** is one TUI session hosted by the engine: its tabs, running commands, workflows and dialogs. A **frontend** is a terminal attached to an instance. The TUI you launch is the instance's primary frontend; when it quits, the instance ends the same way a builtin TUI would, with the same prompts about running work.

List the instances on the daemon, optionally filtered by annotation:

```sh
awman daemon api instances.list
awman daemon api instances.list annotation.workspace=demo
awman daemon api instances.get id=ins_…
awman daemon api dialogs.list id=ins_…
```

`awman daemon api --list` shows every action with its route, including `dialogs.answer` (answer a pending dialog by ID) and `ui.request` (ask an instance to open or close a tab or change focus). Engine routes are served on the local listener only. See [API & Remote Mode](09-api-and-remote-mode.md#engine-routes).

## Upgrading awman

The TUI and the daemon must be the same awman build to share an instance. If you start a newer awman while an older daemon is running:

- if the old daemon is idle, awman restarts it silently and continues;
- if it is running jobs, the TUI asks `Restart it now? Running work will be stopped.` before opening. Answer `y` to restart; `n`, Esc or Ctrl-C decline (Enter does not count as yes). Declining exits with status 2. The CLI prints the same advice instead of prompting.

## When the engine is unavailable

With `tui.backend=auto`, any failure to reach the engine (the daemon cannot start, the engine is off with `allowEphemeral=false`, the engine is shutting down) falls back to the builtin backend and shows a one-line notice in the status bar:

```
engine: unavailable: the daemon's engine is off and daemon.features.engine.allowEphemeral is false
```

With `tui.backend=daemon` the same failure is fatal and printed to stderr.

## Integrations

An **integration** is a tool that works alongside awman's TUI and can ask for it to run on the daemon engine (for example because it wants to watch or drive your instances). Integrations are detected automatically. You can switch one per repository or per name with `tui.integrations.<name>.enabled`:

| Value | Meaning |
|-------|---------|
| `auto` (default) | Use the integration when it detects that it applies. |
| `on` | Always use it. |
| `off` | Never use it; it will not pull the TUI onto the daemon. |

```sh
awman config set tui.integrations.<name>.enabled off
awman --set tui.integrations.<name>.enabled=on
```

The environment variable form is `AWMAN_TUI_INTEGRATIONS_<NAME>_ENABLED`.

### Annotations

An instance (and each of its tabs) can carry **annotations**: small `key=value` labels. Integrations use them to say which instance belongs to what, so a tool can find "the awman instance for this workspace" with `awman daemon api instances.list annotation.<key>=<value>`. Each integration writes only under its own namespace, and annotations are labels only; they do not change what a command does, so do not put secrets in them. Annotations disappear with the instance.

When an instance wants to show something integration-specific, such as opening a tab, an integration can claim the request; if no integration claims it, awman handles opening and closing tabs itself and ignores the rest.
