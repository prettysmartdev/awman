# Command Reference

Generated from `CommandCatalogue` (`src/command/dispatch/catalogue.rs`) — do not hand-edit. Every command, flag, and argument awman accepts is listed here exactly as the catalogue defines it.

---

## `awman init`

Initialize the current Git repo for use with awman.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--agent` | string | `claude` | — | — | CLI, TUI, API | Code agent to install in the Dockerfile.dev container: claude, codex, opencode, gemini, copilot, cline, antigravity, or the name of a custom agent defined in .awman/agents/ or ~/.awman/agents/. |
| `--aspec` | bool | false | — | — | CLI, TUI, API | Download aspec templates to the current project. |

---

## `awman ready`

Check Docker daemon, verify Dockerfile.dev, build image, and report status.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--refresh` | bool | false | — | — | CLI, TUI, API | Run the Dockerfile agent audit (skipped by default). |
| `--build` | bool | false | — | — | CLI, TUI, API | Force rebuild the dev container image from Dockerfile.dev. |
| `--no-cache` | bool | false | — | — | CLI, TUI, API | Skip the build cache when a build is scheduled, and on the builtin runtime also force a fresh image import. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode instead of interactive mode. |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |
| `--json` | bool | false | `--non-interactive` | — | CLI, TUI, API | Suppress human output and print structured JSON. Implies --non-interactive. |
| `--agent` | string (optional) | — | — | — | CLI, TUI, API | Agent to check and build (overrides .awman/config.json). Reviews and approves a custom agent definition that needs approval. |

---

## `awman chat`

Start a freeform chat session with the configured agent in a container.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode. |
| `--plan` | bool | false | — | `--yolo` | CLI, TUI, API | Run the agent in plan mode (read-only). |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |
| `--launch-mode` | enum: stdio, acp | — | — | — | CLI, TUI, API | Launch the agent over stdio or ACP. |
| `--yolo` | bool | false | — | `--plan` | CLI, TUI, API | Enable fully autonomous mode. |
| `--auto` | bool | false | — | — | CLI, TUI, API | Enable auto permission mode. |
| `--agent` | string (optional) | — | — | — | CLI, TUI, API | Agent to use (overrides .awman/config.json). |
| `--model` | string (optional) | — | — | — | CLI, TUI, API | Override the model used by the launched agent. |
| `--overlay` | string (repeatable) | [] | — | — | CLI, TUI, API | Mount a host directory into the agent container. Repeatable. |
| `--agent-flag` | string (repeatable) | [] | — | — | CLI, TUI, API | Repeatable extra argument for the agent command, replacing its agentFlags entry (pass as --agent-flag=--verbose). |

---

## `awman specs`

Manage work item specs (amend).

Available via: CLI, TUI

### `awman specs amend`

Review and amend a completed work item to match the final implementation.

Available via: CLI, TUI

| Argument | Kind | Required | Help |
|---|---|---|---|
| `work_item` | string | yes | Work item number (e.g. 0025). |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode. |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |

---

## `awman agents`

Inspect and manage running code agents.

Available via: CLI, TUI

### `awman agents status`

Show the status of running code agents.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--watch` | bool | false | — | `--json` | CLI, TUI | Keep refreshing the dashboard until interrupted. |
| `--json` | bool | false | — | `--watch` | CLI, TUI, API | Emit JSON output. |

### `awman agents list`

Alias: `ps`, `ls`

List running code agents.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-a, --all` | bool | false | — | — | CLI, TUI, API | Include stopped agents that are still eligible for cleanup. |
| `-q, --quiet` | bool | false | — | `--json` | CLI, TUI, API | Print only agent IDs. |
| `--json` | bool | false | — | `--quiet` | CLI, TUI, API | Emit JSON output. |
| `--no-trunc` | bool | false | — | — | CLI, TUI, API | Do not truncate IDs, names or image names. |

### `awman agents detail`

Alias: `inspect`

Show details for an agent.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `id` | string | yes | Agent ID, unique ID prefix, or name. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--json` | bool | false | — | — | CLI, TUI, API | Emit JSON output. |

### `awman agents kill`

Alias: `rm`

Stop and remove a running agent.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `id` | string | yes | Agent ID, unique ID prefix, or name. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--json` | bool | false | — | — | CLI, TUI, API | Emit JSON output. |
| `-y, --yes` | bool | false | — | — | CLI, TUI, API | Skip the confirmation prompt (for scripting). |

### `awman agents attach`

Attach to an agent's terminal.

Available via: CLI, TUI

| Argument | Kind | Required | Help |
|---|---|---|---|
| `id` | string | yes | Agent ID, unique ID prefix, or name. |

---

## `awman config`

View and edit global and repo configuration.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

### `awman config show`

Display all config fields at both global and repo level.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

### `awman config get`

Show a single field's global value, repo value, and effective value.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

| Argument | Kind | Required | Help |
|---|---|---|---|
| `field` | string | yes | Config field name (e.g. terminal_scrollback_lines). |

### `awman config set`

Set a config field value (repo scope by default).

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

| Argument | Kind | Required | Help |
|---|---|---|---|
| `field` | string | yes | Config field name. |
| `value` | string | yes | New value for the field. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--global` | bool | false | — | — | CLI, TUI, API | Write to global config instead of repo config. |

### `awman config migrate`

Move settings awman has renamed to their current keys in the repo config, after confirmation.

Runs every known config migration for the chosen scope. Today that is one: `dynamicWorkflows` moves to `workflows.dynamic`. Shows the before/after JSON and asks first; teammates on awman versions before this release stop seeing migrated settings once the change is committed.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-y, --yes` | bool | false | — | — | CLI, TUI, API | Skip the confirmation prompt (for scripting). |
| `--dry-run` | bool | false | — | — | CLI, TUI, API | Show what would change without writing anything. |
| `--global` | bool | false | — | — | CLI, TUI, API | Migrate the global config instead of the repo config. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Never prompt: refuse instead of asking for confirmation (pass --yes to proceed). |

### `awman config prerequisite-grants`

List or revoke remembered workflow prerequisite decisions (SSH key, token variable).

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

#### `awman config prerequisite-grants list`

List remembered workflow prerequisite decisions. They hold decisions only, never secrets.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

#### `awman config prerequisite-grants revoke`

Revoke a remembered workflow prerequisite decision.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

| Argument | Kind | Required | Help |
|---|---|---|---|
| `selector` | string | yes | The record number shown by `config prerequisite-grants list`, or all. |

---

## `awman exec`

Run a one-shot command: inject a prompt or run a workflow without a work item.

Available via: CLI, TUI

### `awman exec prompt`

Send a one-shot prompt to the agent.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `prompt` | trailing args | no | The prompt text to send to the agent. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode. |
| `--plan` | bool | false | — | `--yolo` | CLI, TUI, API | Run the agent in plan mode (read-only). |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |
| `--launch-mode` | enum: stdio, acp | — | — | — | CLI, TUI, API | Launch the agent over stdio or ACP. |
| `--yolo` | bool | false | — | `--plan` | CLI, TUI, API | Enable fully autonomous mode. |
| `--auto` | bool | false | — | — | CLI, TUI, API | Enable auto permission mode. |
| `--agent` | string (optional) | — | — | — | CLI, TUI, API | Agent to use (overrides .awman/config.json). |
| `--model` | string (optional) | — | — | — | CLI, TUI, API | Override the model used by the launched agent. |
| `--overlay` | string (repeatable) | [] | — | — | CLI, TUI, API | Mount a host directory into the agent container. Repeatable. |
| `--agent-flag` | string (repeatable) | [] | — | — | CLI, TUI, API | Repeatable extra argument for the agent command, replacing its agentFlags entry (pass as --agent-flag=--verbose). |
| `--issue` | string (optional) | — | — | — | CLI, TUI, API | GitHub issue number, URL, or owner/repo#N to use as the prompt. |

### `awman exec workflow`

Alias: `wf`

Run a workflow file without requiring a work item number.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `workflow` | path | no | Path to the workflow file (omit with --dynamic). |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--work-item` | string (optional) | — | — | `--issue` | CLI, TUI, API | Optional work item number. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode. |
| `--plan` | bool | false | — | `--yolo` | CLI, TUI, API | Run the agent in plan mode (read-only). |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |
| `--launch-mode` | enum: stdio, acp | — | — | — | CLI, TUI, API | Launch the agent over stdio or ACP. |
| `--worktree` | bool | false | — | — | CLI, TUI, API | Run in an isolated Git worktree under ~/.awman/worktrees/. |
| `--yolo` | bool | false | `--worktree` | `--plan` | CLI, TUI, API | Enable fully autonomous mode. Implies --worktree. |
| `--auto` | bool | false | `--worktree` | — | CLI, TUI, API | Enable auto permission mode. Implies --worktree. |
| `--agent` | string (optional) | — | — | — | CLI, TUI, API | Agent to use. |
| `--model` | string (optional) | — | — | — | CLI, TUI, API | Override the model used by the launched agent. |
| `--overlay` | string (repeatable) | [] | — | — | CLI, TUI, API | Mount a host directory into the agent container. Repeatable. |
| `--agent-flag` | string (repeatable) | [] | — | — | CLI, TUI, API | Repeatable extra argument for the agent command, replacing its agentFlags entry (pass as --agent-flag=--verbose). |
| `--issue` | string (optional) | — | — | `--work-item` | CLI, TUI, API | GitHub issue number, URL, or owner/repo#N to use as work item input. |
| `--dynamic` | bool | false | — | — | CLI, TUI, API | Have a leader agent design and run a workflow for --work-item. Implies --yolo, --worktree, and context(workflow); the positional workflow path must be omitted. |
| `--leader` | string (optional) | — | — | — | CLI, TUI, API | Agent and model for the dynamic leader, as agent::model (e.g. claude::claude-opus-4-8). Only valid with --dynamic. |
| `--max-concurrent` | usize (>= 1) | — | — | — | CLI, TUI, API | Cap on concurrently-running workflow steps (must be >= 1). |
| `--on-dirty-merge-target` | enum: ask, commit, stash, fail | — | — | — | CLI, TUI, API | What to do when the merge target has uncommitted changes before a worktree branch is merged back. |
| `--on-merge-failure` | enum: ask, agent, keep | — | — | — | CLI, TUI, API | What to do when merging a worktree branch back fails: ask, launch a fix agent, or keep the worktree. |
| `--on-existing-worktree` | enum: ask, resume, recreate, fail | — | — | — | CLI, TUI, API | What to do when the worktree for this run already exists: ask, resume it, recreate it (deletes the worktree, its branch and that branch's commits), or fail. |
| `--prerequisite-identity` | string (optional) | — | — | — | CLI, TUI, API | Git identity for commit_changes: host (your global git identity), "Name <email>", or deny. Never granted by --yes. |
| `--prerequisite-ssh` | string (optional) | — | — | — | CLI, TUI, API | SSH key for push_branch: a key path under ~/.ssh (mounted read-only), all (the whole ~/.ssh), or deny. Never granted by --yes. |
| `--prerequisite-token` | string (optional) | — | — | — | CLI, TUI, API | GitHub token for create_pull_request, an HTTPS push_branch and poll_ci: env:NAME, acquire (the configured token command), or deny. Never granted by --yes. |
| `--prerequisite-unresolved` | enum: warn, fail | — | — | — | CLI, TUI, API | What an unsatisfied workflow prerequisite does: warn and continue, or fail before any container starts. |
| `--check-prerequisites` | bool | false | — | — | CLI, TUI, API | Run only the workflow prerequisite checks, print the report and exit 0 when all checks are satisfied or 1 when any check is missing, needs consent or is unknown. |
| `--json` | bool | false | `--non-interactive` | — | CLI, TUI, API | Print the workflow outcome and prerequisite report as JSON. |

---

## `awman api`

Run awman as an API HTTP server for remote/automated access.

Available via: CLI, TUI

### `awman api start`

Start the API HTTP server.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--port` | u16 | 9876 | — | — | CLI | Port to listen on. |
| `--workdirs` | string (repeatable) | [] | — | — | CLI | Allowlisted working directories (repeatable). |
| `--workdir-root` | string (repeatable) | [] | — | — | CLI | Allowlisted root directories; any git repository cloned directly beneath one may be used as a session workdir (repeatable). |
| `--background` | bool | false | — | — | CLI | Daemonize via the OS process manager. |
| `--refresh-key` | bool | false | — | — | CLI | Regenerate the API key. |
| `--dangerously-skip-auth` | bool | false | — | — | CLI | Disable authentication for this execution even if a key hash exists on disk. |
| `--dangerously-skip-tls` | bool | false | — | — | CLI | Serve plain HTTP instead of HTTPS. Intended for localhost/test only. |

### `awman api kill`

Stop the background API server.

Available via: CLI, TUI

### `awman api logs`

Stream the background server log file to stdout.

Available via: CLI, TUI

### `awman api status`

Show API server status.

Available via: CLI, TUI

---

## `awman squad`

Manage the squad task daemon and scheduled tasks.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI | Print the squad status summary instead of opening the TUI. |
| `--json` | bool | false | `--non-interactive` | — | CLI, TUI, API | Emit JSON output. |

### `awman squad start`

Start the squad daemon.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--port` | u16 | 0 | — | — | CLI, TUI, API | Port to listen on (0 selects an OS-assigned port). |
| `--background` | bool | false | — | — | CLI, TUI, API | Daemonize via the OS process manager. |
| `--refresh-key` | bool | false | — | — | CLI, TUI, API | Regenerate the squad key and print its AWMAN_SQUAD_KEY export snippet. |
| `--dangerously-skip-auth` | bool | false | — | — | CLI, TUI, API | Skip key creation and authentication for this run (loopback-only). |

### `awman squad stop`

Alias: `kill`

Stop the squad daemon.

Available via: CLI, TUI, API

### `awman squad status`

Show squad daemon status.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--json` | bool | false | — | — | CLI, TUI, API | Emit JSON output. |

### `awman squad logs`

Show the squad daemon log.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-f, --follow` | bool | false | — | — | CLI, TUI, API | Follow the log as it grows. |

### `awman squad add`

Create a squad task.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--name` | string | — | — | — | CLI, TUI, API | Task slug. |
| `--description` | string | — | — | — | CLI, TUI, API | Task description. |
| `--repo` | path | — | — | — | CLI, TUI, API | Legacy synonym for `--workspace <path>`; ignored when `--workspace` is given. |
| `--interval` | string | `6h` | — | — | CLI, TUI, API | Evaluation interval (for example 6h). |
| `--agent` | string | — | — | — | CLI, TUI, API | Task-specific leader agent. |
| `--model` | string | — | — | — | CLI, TUI, API | Task-specific leader model. |
| `--workspace` | string | — | — | — | CLI, TUI, API | Task workspace: `default` for the durable per-task workspace, or a folder/repo path. Defaults to `default`. |
| `--overlay` | string (repeatable) | [] | — | — | CLI, TUI, API | Overlay the task's containers get: dir()/ssh()/env()/skill(). Repeatable. |
| `--mount-scope` | enum: cwd, gitroot | `gitroot` | — | — | CLI, TUI, API | Repository scope mounted for scheduled runs (custom git-repo workspaces only). |
| `--agent-models` | string (repeatable) | [] | — | — | CLI, TUI, API | Agents and models this task may use: <agent>=<model>[,<model>...]. Repeatable. |
| `--interview` | bool | false | — | `--non-interactive` | CLI, TUI | Collect task fields interactively. |
| `-n, --non-interactive` | bool | false | — | `--interview` | CLI, TUI | Never prompt: refuse anything needing a confirmation instead of asking. |
| `--prerequisite-identity` | string (optional) | — | — | — | CLI, TUI, API | Git identity for commit_changes: host (your global git identity), "Name <email>", or deny. Never granted by --yes. |
| `--prerequisite-ssh` | string (optional) | — | — | — | CLI, TUI, API | SSH key for push_branch: a key path under ~/.ssh (mounted read-only), all (the whole ~/.ssh), or deny. Never granted by --yes. |
| `--prerequisite-token` | string (optional) | — | — | — | CLI, TUI, API | GitHub token for create_pull_request, an HTTPS push_branch and poll_ci: env:NAME, acquire (the configured token command), or deny. Never granted by --yes. |
| `--prerequisite-unresolved` | enum: warn, fail | — | — | — | CLI, TUI, API | What an unsatisfied workflow prerequisite does: warn and continue, or fail before any container starts. |

### `awman squad edit`

Edit an existing squad task.

Change a task's description, schedule, leader agent/model, overlays, or agent pool. A task's name, workspace and mount scope are fixed at creation and cannot be edited; changing those means creating a new task. Every flag is optional, but at least one must be given unless --interview is used.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--description` | string | — | — | — | CLI, TUI, API | Replace the task description. |
| `--interval` | string | — | — | — | CLI, TUI, API | Replace the evaluation interval (for example 6h). |
| `--agent` | string | — | — | `--clear-agent` | CLI, TUI, API | Replace the task-specific leader agent. |
| `--clear-agent` | bool | false | — | `--agent` | CLI, TUI, API | Drop the task's own leader agent, falling back to the squad default. |
| `--model` | string | — | — | `--clear-model` | CLI, TUI, API | Replace the task-specific leader model. |
| `--clear-model` | bool | false | — | `--model` | CLI, TUI, API | Drop the task's own leader model, falling back to the squad default. |
| `--overlay` | string (repeatable) | [] | — | `--clear-overlays` | CLI, TUI, API | Replace the task's overlays: dir()/ssh()/env()/skill(). Repeatable. |
| `--clear-overlays` | bool | false | — | `--overlay` | CLI, TUI, API | Remove every overlay from the task. |
| `--agent-models` | string (repeatable) | [] | — | — | CLI, TUI, API | Agents and models this task may use: <agent>=<model>[,<model>...]. Repeatable. |
| `--clear-agent-models` | bool | false | — | `--agent-models` | CLI, TUI, API | Remove the task's own agent pool, inheriting the global squad settings. |
| `--interview` | bool | false | — | `--non-interactive` | CLI, TUI | Collect the edited fields interactively, prefilled with the current values. |
| `-n, --non-interactive` | bool | false | — | `--interview` | CLI, TUI | Never prompt: refuse anything needing a confirmation instead of asking. |
| `--prerequisite-identity` | string (optional) | — | — | — | CLI, TUI, API | Git identity for commit_changes: host (your global git identity), "Name <email>", or deny. Never granted by --yes. |
| `--prerequisite-ssh` | string (optional) | — | — | — | CLI, TUI, API | SSH key for push_branch: a key path under ~/.ssh (mounted read-only), all (the whole ~/.ssh), or deny. Never granted by --yes. |
| `--prerequisite-token` | string (optional) | — | — | — | CLI, TUI, API | GitHub token for create_pull_request, an HTTPS push_branch and poll_ci: env:NAME, acquire (the configured token command), or deny. Never granted by --yes. |
| `--prerequisite-unresolved` | enum: warn, fail | — | — | — | CLI, TUI, API | What an unsatisfied workflow prerequisite does: warn and continue, or fail before any container starts. |

### `awman squad list`

List squad tasks.

Available via: CLI, TUI, API

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--json` | bool | false | — | — | CLI, TUI, API | Emit JSON output. |

### `awman squad show`

Show a squad task.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--json` | bool | false | — | — | CLI, TUI, API | Emit JSON output. |

### `awman squad remove`

Remove a squad task.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-y, --yes` | bool | false | — | — | CLI, TUI | Do not prompt for confirmation. |

### `awman squad pause`

Pause a squad task.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

### `awman squad resume`

Resume a squad task.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

### `awman squad trigger`

Evaluate a squad task now, ignoring its schedule.

Ask the squad daemon to evaluate a task on its next scheduler tick, whatever its interval says and whatever backoff is outstanding. The task's interval is not changed: the trigger fires exactly one evaluation, after which the task returns to its normal schedule. A paused task is refused — resume it first.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

### `awman squad cancel`

Cancel a squad task's in-progress run.

Stop the run a squad task is executing right now: its evaluation is abandoned, every agent container it started is stopped, and the run is recorded as canceled in the task's history. The task keeps its schedule and is evaluated again when next due. Fails when the task has no run in progress.

Available via: CLI, TUI, API

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

### `awman squad attach`

Attach to a running squad task container.

Available via: CLI, TUI

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Task name. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--container` | string | — | — | — | CLI, TUI | Running container ID when multiple are active. |

### `awman squad env`

Show which env() values the squad daemon has, and where they came from.

Report every environment variable the squad daemon needs — the union of every env(NAME) overlay across the task store, the daemon's own config and AWMAN_OVERLAYS — with whether the daemon currently holds a value, where that value came from (this shell, a previous push, or the OS keychain at startup), and how long any missing one has been missing.

Values are never printed: this command reports only whether one is present. Running it with no flag also performs the ordinary coverage check, which sends a value only when it actually differs from what the daemon holds. --push re-sends every value this shell has regardless, which is what to reach for after rotating a token. --clear removes the daemon's persisted keychain item; the running daemon keeps the values it already holds.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--push` | bool | false | — | — | CLI, TUI, API | Push every required value this shell has, whether or not it differs. |
| `--clear` | bool | false | — | — | CLI, TUI, API | Remove the daemon's persisted keychain item. |
| `--json` | bool | false | — | — | CLI, TUI, API | Emit JSON output. |

---

## `awman remote`

Connect to a remote awman API instance and execute commands.

Available via: CLI, TUI

### `awman remote session`

Manage sessions on the remote awman API host.

Available via: CLI, TUI

#### `awman remote session start`

Start a new session on the remote host.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--remote-addr` | string (optional) | — | — | — | CLI, TUI, API | Address of the remote awman API host. |
| `--api-key` | string (optional) | — | — | — | CLI, TUI, API | API key for the remote awman API host. |
| `--type` | enum: local, remote | `local` | — | — | CLI, TUI, API | Session type: 'local' or 'remote'. |
| `--workdir` | string (optional) | — | — | — | CLI, TUI, API | Working directory (required for --type local). |
| `--repo-url` | string (optional) | — | — | — | CLI, TUI, API | Repository URL (required for --type remote). |
| `--branch` | string (optional) | — | — | — | CLI, TUI, API | Branch name (optional, for --type remote). |

#### `awman remote session kill`

Kill a session on the remote host.

Available via: CLI, TUI

| Argument | Kind | Required | Help |
|---|---|---|---|
| `session_id` | string (optional) | no | Session ID to kill. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--remote-addr` | string (optional) | — | — | — | CLI, TUI, API | Address of the remote awman API host. |
| `--api-key` | string (optional) | — | — | — | CLI, TUI, API | API key for the remote awman API host. |

### `awman remote exec`

Execute a command on the remote awman API host.

Available via: CLI, TUI

#### `awman remote exec workflow`

Alias: `wf`

Submit a workflow for execution on the remote host.

Available via: CLI, TUI

| Argument | Kind | Required | Help |
|---|---|---|---|
| `workflow` | path | yes | Path to the workflow file. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--remote-addr` | string (optional) | — | — | — | CLI, TUI, API | Address of the remote awman API host. |
| `--session` | string (optional) | — | — | — | CLI, TUI, API | Session ID to use on the remote host. |
| `--api-key` | string (optional) | — | — | — | CLI, TUI, API | API key for the remote awman API host. |
| `-f, --follow` | bool | false | — | — | CLI, TUI, API | Stream logs via SSE until the command completes. |
| `--work-item` | string (optional) | — | — | `--issue` | CLI, TUI, API | Optional work item number. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode. |
| `--plan` | bool | false | — | `--yolo` | CLI, TUI, API | Run the agent in plan mode (read-only). |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |
| `--launch-mode` | enum: stdio, acp | — | — | — | CLI, TUI, API | Launch the agent over stdio or ACP. |
| `--yolo` | bool | false | `--worktree` | `--plan` | CLI, TUI, API | Enable fully autonomous mode. Implies --worktree. |
| `--auto` | bool | false | `--worktree` | — | CLI, TUI, API | Enable auto permission mode. Implies --worktree. |
| `--agent` | string (optional) | — | — | — | CLI, TUI, API | Agent to use. |
| `--model` | string (optional) | — | — | — | CLI, TUI, API | Override the model used by the launched agent. |
| `--overlay` | string (repeatable) | [] | — | — | CLI, TUI, API | Mount a host directory into the agent container. Repeatable. |
| `--agent-flag` | string (repeatable) | [] | — | — | CLI, TUI, API | Repeatable extra argument for the agent command, replacing its agentFlags entry (pass as --agent-flag=--verbose). |
| `--issue` | string (optional) | — | — | `--work-item` | CLI, TUI, API | GitHub issue number, URL, or owner/repo#N to use as work item input. |
| `--dynamic` | bool | false | — | — | CLI, TUI, API | Have a leader agent design and run a workflow for --work-item. Implies --yolo, --worktree, and context(workflow); the positional workflow path must be omitted. |
| `--leader` | string (optional) | — | — | — | CLI, TUI, API | Agent and model for the dynamic leader, as agent::model (e.g. claude::claude-opus-4-8). Only valid with --dynamic. |
| `--max-concurrent` | usize (>= 1) | — | — | — | CLI, TUI, API | Cap on concurrently-running workflow steps (must be >= 1). |
| `--on-dirty-merge-target` | enum: ask, commit, stash, fail | — | — | — | CLI, TUI, API | What to do when the merge target has uncommitted changes before a worktree branch is merged back. |
| `--on-merge-failure` | enum: ask, agent, keep | — | — | — | CLI, TUI, API | What to do when merging a worktree branch back fails: ask, launch a fix agent, or keep the worktree. |
| `--on-existing-worktree` | enum: ask, resume, recreate, fail | — | — | — | CLI, TUI, API | What to do when the worktree for this run already exists: ask, resume it, recreate it (deletes the worktree, its branch and that branch's commits), or fail. |
| `--prerequisite-identity` | string (optional) | — | — | — | CLI, TUI, API | Git identity for commit_changes: host (your global git identity), "Name <email>", or deny. Never granted by --yes. |
| `--prerequisite-ssh` | string (optional) | — | — | — | CLI, TUI, API | SSH key for push_branch: a key path under ~/.ssh (mounted read-only), all (the whole ~/.ssh), or deny. Never granted by --yes. |
| `--prerequisite-token` | string (optional) | — | — | — | CLI, TUI, API | GitHub token for create_pull_request, an HTTPS push_branch and poll_ci: env:NAME, acquire (the configured token command), or deny. Never granted by --yes. |
| `--prerequisite-unresolved` | enum: warn, fail | — | — | — | CLI, TUI, API | What an unsatisfied workflow prerequisite does: warn and continue, or fail before any container starts. |
| `--check-prerequisites` | bool | false | — | — | CLI, TUI, API | Run only the workflow prerequisite checks, print the report and exit 0 when all checks are satisfied or 1 when any check is missing, needs consent or is unknown. |
| `--json` | bool | false | `--non-interactive` | — | CLI, TUI, API | Print the workflow outcome and prerequisite report as JSON. |

#### `awman remote exec prompt`

Send a one-shot prompt to the remote host.

Available via: CLI, TUI

| Argument | Kind | Required | Help |
|---|---|---|---|
| `prompt` | trailing args | yes | The prompt text to send to the agent. |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--remote-addr` | string (optional) | — | — | — | CLI, TUI, API | Address of the remote awman API host. |
| `--session` | string (optional) | — | — | — | CLI, TUI, API | Session ID to use on the remote host. |
| `--api-key` | string (optional) | — | — | — | CLI, TUI, API | API key for the remote awman API host. |
| `-f, --follow` | bool | false | — | — | CLI, TUI, API | Stream logs via SSE until the command completes. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the agent in non-interactive (print) mode. |
| `--plan` | bool | false | — | `--yolo` | CLI, TUI, API | Run the agent in plan mode (read-only). |
| `--allow-docker` | bool | false | — | — | CLI, TUI, API | Mount the host Docker daemon socket into the agent container. |
| `--launch-mode` | enum: stdio, acp | — | — | — | CLI, TUI, API | Launch the agent over stdio or ACP. |
| `--yolo` | bool | false | — | `--plan` | CLI, TUI, API | Enable fully autonomous mode. |
| `--auto` | bool | false | — | — | CLI, TUI, API | Enable auto permission mode. |
| `--agent` | string (optional) | — | — | — | CLI, TUI, API | Agent to use (overrides .awman/config.json). |
| `--model` | string (optional) | — | — | — | CLI, TUI, API | Override the model used by the launched agent. |
| `--overlay` | string (repeatable) | [] | — | — | CLI, TUI, API | Mount a host directory into the agent container. Repeatable. |
| `--agent-flag` | string (repeatable) | [] | — | — | CLI, TUI, API | Repeatable extra argument for the agent command, replacing its agentFlags entry (pass as --agent-flag=--verbose). |
| `--issue` | string (optional) | — | — | — | CLI, TUI, API | GitHub issue number, URL, or owner/repo#N to use as the prompt. |

---

## `awman new`

Create a new awman artefact (spec, workflow, skill, or agent definition).

Available via: CLI, TUI

### `awman new spec`

Create a new work item spec.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--interview` | bool | false | — | — | CLI, TUI, API | Use interview mode. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the interview agent in non-interactive (print) mode. |
| `--issue` | string (optional) | — | — | — | CLI, TUI, API | GitHub issue number, URL, or owner/repo#N to use as spec input. |

### `awman new workflow`

Interactively create a new workflow file.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--interview` | bool | false | — | — | CLI, TUI, API | Let a code agent complete the workflow from a summary you provide. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the interview agent in non-interactive (print) mode. |
| `--global` | bool | false | — | — | CLI, TUI, API | Write to ~/.awman/workflows/<name> instead of the current repo. |
| `--format` | enum: toml, yaml | `toml` | — | — | CLI, TUI, API | Output file format. |

### `awman new skill`

Interactively create a new skill file.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--interview` | bool | false | — | — | CLI, TUI, API | Let a code agent complete the skill body from a summary you provide. |
| `-n, --non-interactive` | bool | false | — | — | CLI, TUI, API | Run the interview agent in non-interactive (print) mode. |
| `--global` | bool | false | — | — | CLI, TUI, API | Write to ~/.awman/skills/<name>/ instead of the current repo. |
| `--pull` | string (optional) | — | — | `--pull-all`, `--interview`, `--global` | CLI, TUI, API | Pull (or refresh) a published skills library from GitHub, e.g. github.com/obra/superpowers. |
| `--pull-all` | bool | false | — | `--pull`, `--subdir`, `--interview`, `--global` | CLI, TUI, API | Refresh every previously-pulled skills library. |
| `--subdir` | string (optional) | — | — | `--pull-all` | CLI, TUI, API | Subdirectory inside the pulled repo containing skills (default: skills). |

### `awman new agent`

Create a custom agent definition in .awman/agents/<name>.toml.

Available via: CLI, TUI

Runs without a working agent runtime — reachable even when the configured runtime cannot be started on this host.

| Argument | Kind | Required | Help |
|---|---|---|---|
| `name` | string | yes | Agent name; becomes the file stem (lowercase letters, digits, '-' and '_'). |

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `--global` | bool | false | — | — | CLI, TUI, API | Write to ~/.awman/agents/<name>.toml instead of the current repo. |
| `--from` | enum: crush, maki | — | — | — | CLI, TUI, API | Copy a shipped example definition instead of the commented skeleton. |
| `--force` | bool | false | — | — | CLI, TUI, API | Overwrite an existing definition (and, with --from maki, its Dockerfile). |

---

## `awman clean`

Remove stopped awman containers, completed workflow data, and dangling images.

Available via: CLI, TUI

| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |
|---|---|---|---|---|---|---|
| `-y, --yes` | bool | false | — | — | CLI, TUI, API | Skip the confirmation prompt (for scripting). |
| `--dry-run` | bool | false | — | — | CLI, TUI, API | List what would be removed without deleting anything. |

---

[← Cleaning Up](13-cleaning-up.md) · [Back to contents](contents.md)
