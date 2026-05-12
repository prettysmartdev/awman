# CLI Design

Binary name: `amux`
Install path: `/usr/local/bin/`
Storage location: `$HOME/.amux/`

This document is the authoritative specification of the `amux` CLI surface. It is regenerated from `CommandCatalogue` (see `src/command/dispatch/catalogue.rs`); when you change a command, subcommand, flag, or alias, update this file. CI does not block on drift today, but every reviewer should treat divergence between this file and the catalogue as a defect.

## Design principles

- **Single binary, two modes.** `amux` with no arguments launches a Ratatui TUI. `amux <subcommand> …` runs a single command and exits, with output on stdout/stderr.
- **Catalogue-driven.** Every flag, subcommand, and default lives in `CommandCatalogue`. Frontends read from the catalogue rather than hard-coding strings.
- **Non-interactive by default for scripts.** Flags like `--non-interactive` and `--json` are first-class for headless and CI use. `--json` always implies `--non-interactive`.
- **Container isolation.** Every agentic operation runs inside a Docker (or Apple Containers) container built from `Dockerfile.dev`. The host never executes agent code directly.

## Top-level commands

| Command | Summary |
|---|---|
| `amux` | Launch the interactive TUI. |
| `amux init` | Initialize the current Git repo for use with amux. |
| `amux ready` | Verify the Docker daemon, ensure `Dockerfile.dev`, build the dev image. |
| `amux chat` | Freeform chat session with the configured agent. |
| `amux specs <subcommand>` | Manage work item specs. |
| `amux new <subcommand>` | Create a new amux artefact (spec, workflow, skill). |
| `amux exec <subcommand>` | Run a one-shot prompt or workflow. |
| `amux config <subcommand>` | View and edit global/repo configuration. |
| `amux status` | Show all running amux containers. |
| `amux headless <subcommand>` | Run amux as a headless HTTP server. |
| `amux remote <subcommand>` | Connect to a remote headless instance. |

### Top-level flags (apply before any subcommand)

| Flag | Kind | Default | Description |
|---|---|---|---|
| `--build` | bool | false | Force rebuild of images on startup. |
| `--no-cache` | bool | false | Disable Docker layer cache during builds. |
| `--refresh` | bool | false | Refresh agent environment (run audit). |
| `-h, --help` | bool | — | Print help. |
| `-V, --version` | bool | — | Print version. |

## Per-command surface

### `amux init`

Initialize the current Git repo for use with amux.

| Flag | Kind | Default | Description |
|---|---|---|---|
| `--agent <name>` | enum | `claude` | One of: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`. |
| `--aspec` | bool | false | Download aspec templates into the project. |

### `amux ready`

| Flag | Kind | Default | Description |
|---|---|---|---|
| `--refresh` | bool | false | Run the Dockerfile agent audit. |
| `--build` | bool | false | Force rebuild of the dev image. |
| `--no-cache` | bool | false | Pass `--no-cache` to `docker build`. |
| `-n, --non-interactive` | bool | false | Run the agent in non-interactive (print) mode. |
| `--allow-docker` | bool | false | Mount the host Docker daemon socket into the agent container. |
| `--json` | bool | false | Suppress human output and print structured JSON. **Implies `--non-interactive`.** |

### `amux chat`

| Flag | Kind | Default | Description |
|---|---|---|---|
| `-n, --non-interactive` | bool | false | Non-interactive (print) mode. |
| `--plan` | bool | false | Plan mode (read-only). |
| `--allow-docker` | bool | false | Mount the host Docker daemon socket. |
| `--mount-ssh` | bool | false | Mount host `~/.ssh` read-only. |
| `--yolo` | bool | false | Fully autonomous mode. |
| `--auto` | bool | false | Auto permission mode. |
| `--agent <name>` | string | — | Override the agent for this run. |
| `--model <name>` | string | — | Override the model for this run. |
| `--overlay <spec>` | repeatable string | — | Mount a host directory into the container. |

### `amux specs`

| Subcommand | Arguments | Flags |
|---|---|---|
| `amend <work_item>` | `<work_item>` | `-n/--non-interactive`, `--allow-docker` |

### `amux new`

| Subcommand | Arguments | Flags |
|---|---|---|
| `spec` | — | `--interview`, `-n/--non-interactive`. |
| `workflow` | — | `--interview`, `-n/--non-interactive`, `--global`, `--format <toml\|yaml\|md>` (default `toml`). |
| `skill` | — | `--interview`, `-n/--non-interactive`, `--global`. |

### `amux exec`

| Subcommand | Arguments | Flags |
|---|---|---|
| `prompt <prompt>` | `<prompt>` | `-n/--non-interactive`, `--plan`, `--allow-docker`, `--mount-ssh`, `--yolo`, `--auto`, `--agent <name>`, `--model <name>`, `--overlay <spec>` (repeatable). |
| `workflow <path>` (alias `wf`) | `<path>` | `--work-item <num>`, `-n/--non-interactive`, `--plan`, `--allow-docker`, `--worktree`, `--mount-ssh`, `--yolo`, `--auto`, `--agent <name>`, `--model <name>`, `--overlay <spec>` (repeatable). `--yolo`/`--auto` imply `--worktree`. |

### `amux config`

| Subcommand | Arguments | Flags |
|---|---|---|
| `show` | — | — |
| `get <field>` | `<field>` | — |
| `set <field> <value>` | `<field>`, `<value>` | `--global` (repo scope by default). |

### `amux status`

| Flag | Description |
|---|---|
| `--watch` | Continuously refresh every 3 seconds. The CLI emits `\x1b[H\x1b[J` clear sequences; the TUI swallows them. |

### `amux headless`

| Subcommand | Flags |
|---|---|
| `start` | `--port <n>` (default `9876`), `--workdirs <path>` (repeatable), `--background`, `--refresh-key`, `--dangerously-skip-auth`. |
| `kill` | — |
| `logs` | — |
| `status` | — |

### `amux remote`

| Subcommand | Arguments | Flags |
|---|---|---|
| `run <command…>` | trailing varargs forwarded verbatim | `--remote-addr <url>`, `--session <id>`, `-f/--follow`, `--api-key <key>`. |
| `session start <dir>` | `<dir>` | — |
| `session kill <session_id>` | `<session_id>` | — |

## Inputs and outputs

- The TUI takes over the terminal via Ratatui; ANSI escapes are forwarded to the agent's PTY.
- CLI commands write human-readable output to stdout and diagnostics to stderr.
- `--json` flips the renderer to a structured-JSON serializer.
- Containers launched by amux plumb the developer's stdin/stdout/stderr through the chosen runtime so the agent runs interactively inside the TUI.

## Configuration

- Per-repo config: `<git-root>/aspec/.amux.json` (and `.amux/config.json` under the project tree).
- Global config: `$HOME/.amux/config.json`.
- Environment overrides: `AMUX_*` variables (notably `AMUX_OVERLAYS`, `AMUX_API_KEY`, `AMUX_HEADLESS_ROOT`).

Precedence (highest to lowest): CLI flag → environment variable → repo config → global config → built-in default.
