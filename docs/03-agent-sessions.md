# Agent Sessions

An agent session is a container (on whichever [runtime](11-runtimes.md) you use) running your configured AI agent (Claude Code, Codex, OpenCode, Gemini, Antigravity, GitHub Copilot CLI, Cline, or any [custom agent](15-custom-agents.md) you define) against your project. awman handles starting the container, injecting your credentials, and connecting your terminal to the agent's input/output.

Sessions normally use the standard stdio launch mode. You can also use
[ACP mode](#acp-launch-mode) when you want awman to render structured agent
activity instead of the agent's raw terminal stream. ACP currently supports
**Cline**, **OpenCode**, and **Gemini CLI**.

---

## Freeform chat

```sh
awman chat
# or, in the TUI command box:
chat
```

`chat` launches an agent with no pre-configured prompt — a clean, blank slate. Use it for exploring the codebase, asking questions, prototyping ideas, or any task where you want to drive the conversation yourself.

In the TUI, the container window opens immediately and all keyboard input is forwarded to the agent. In command mode, the container's stdin/stdout/stderr are directly connected to your terminal.

Press **Ctrl+C** to exit the agent session when you're done.

`awman chat`'s own exit code is the agent's exit code — a session whose agent
crashed or was killed is reported as a failure (non-zero), and the CLI, TUI
tab colour, and any API client watching the command's status all agree on
that result. This matters most for `awman chat --headless` run from a
script or CI job, where the exit code is how the caller finds out whether the
agent succeeded.

---

## ACP launch mode

ACP (Agent Client Protocol) mode gives awman a structured way to present an
agent session. Instead of showing the agent's terminal stream directly, awman
renders the agent's messages, tool activity, plans, and permission requests in
its own interface.

ACP is supported by **Cline**, **OpenCode** (`opencode acp`), and **Gemini CLI**
(`gemini --experimental-acp`). Other agents use the standard stdio launch mode.

ACP is available with the Docker and Apple Containers runtimes. The
`docker-sbx-experimental` runtime does not currently support ACP. The `builtin-experimental` runtime carries ACP over its own binary-clean stream: frames reach the agent byte-for-byte, and closing the client's stdin ends the agent's input as it does on Docker.

The interactive ACP experience currently ships in the **CLI**. See
[ACP in the TUI](#acp-in-the-tui) and [Workflows](#workflows-and-unsupported-agents)
below for the current limits.

### Start an ACP session

Use `--set agents.launchMode=acp` for a single session. Put the flags before the prompt
positional so they are parsed:

```sh
awman chat --agent cline --set agents.launchMode=acp
awman exec prompt --agent cline --set agents.launchMode=acp "Review the failing tests"
```

`--set` is accepted by every command. The shortest form uses your
configured agent, provided it supports ACP:

```sh
awman chat --set agents.launchMode=acp
```

To return to the normal terminal experience for one invocation, use
`--set agents.launchMode=stdio`.

---

### What changes in ACP mode

ACP changes how awman displays and drives the session. You see structured
agent responses and activity through awman's interface, rather than the
agent's raw terminal UI. Permission requests are presented by awman, and an
ACP session can continue with follow-up prompts while it remains open.

The agent still works on the current project through awman's normal isolated
agent session. ACP is a presentation and interaction choice; it does not
change which project you opened or which agent you selected.

---

### ACP in the CLI

The CLI uses an interactive stdio experience for ACP sessions: agent messages
stream to the terminal, while tool activity and other structured updates are
printed as readable blocks. When the agent asks for permission, the CLI shows
the available choices and reads your selection from standard input. After a
turn, it displays a `>` prompt for a follow-up message.

For `exec prompt`, the supplied prompt starts the first turn. For `chat`, you
can begin by entering prompts as the session runs. Press **Ctrl+D** when you
are finished entering follow-up prompts, or **Ctrl+C** to exit the session.

In a non-interactive run (`--headless`, or with `--yolo`/`--auto`), the
CLI does not prompt for follow-ups or permissions — it runs the seeded turn and
exits, matching the headless behavior of stdio agents.

---

### ACP in the TUI

The full TUI agent window for ACP — a dedicated purple-bordered window with
inline structured updates, command-box follow-ups, and dialog permission
prompts — is **not yet available**. Launching an ACP session in the TUI renders
update summaries in the message area and does not present interactive follow-up
or permission dialogs; a permission request that is not pre-approved by
`--yolo`/`--auto` is denied.

For the interactive ACP experience today, run `awman chat --set agents.launchMode=acp`
from the CLI (outside the TUI).

---

### Selecting a launch mode

`agents.launchMode` accepts exactly two values:

```sh
awman chat --agent cline --set agents.launchMode=acp
awman chat --agent cline --set agents.launchMode=stdio
```

For persistent settings, run `awman config set agents.launchMode acp` (repo
config) or set it with `--global`. The default is `stdio`. The effective launch
mode is chosen in this order:

```
--set agents.launchMode  >  AWMAN_AGENTS_LAUNCH_MODE  >  repo agents.launchMode  >  global agents.launchMode  >  stdio
```

See [Configuration](07-configuration.md) for the config fields and the
environment variable.

---

### Workflows and unsupported agents

ACP is **not yet available for `exec workflow` steps**. If a repository sets
`agents.launchMode` is `acp` and a workflow step resolves to an ACP-capable agent, the
workflow fails before any step starts, directing you to run that agent over ACP
with `awman chat` or `awman exec prompt` instead — or to set `agents.launchMode` to `stdio`
for workflows.

Workflow steps whose agents do **not** support ACP are governed by the global
`agents.acpFallback` setting, which defaults to `error`:

- `error` stops the workflow before any step starts and reports the step and
  agent that cannot use ACP.
- `stdio` runs those steps in the regular stdio mode and prints a warning per
  step, and the workflow proceeds.

Set the policy in your global or repo config:

```json
{ "agents": { "acpFallback": "stdio" } }
```

An explicit `--set agents.launchMode=acp` request for a single unsupported agent remains
an error, so a one-off request cannot silently change modes. See
[Workflows](05-workflows.md) for workflow setup and execution.

---

## Flags common to `chat` and other agent-launching commands

### `--agent <name>`

Override the configured agent for this session. Available agents: `claude`, `codex`, `opencode`, `gemini`, `antigravity`, `copilot`, `cline`, plus any [custom agent](15-custom-agents.md).

```sh
# CLI
awman chat --agent codex               # launch a Codex session for this project
awman exec workflow path/to/workflow.toml --agent gemini    # run workflow with Gemini instead of the configured agent
awman chat --agent=copilot             # --flag=value form is also accepted

# TUI command box
chat --agent cline
exec workflow path/to/workflow.toml --agent=cline
```

Both `--agent NAME` and `--agent=NAME` forms are accepted in both the CLI and the TUI command box. The TUI command box honours the flag and passes the correct agent to the container — it is not silently ignored.

This overrides the `agent` field in your repo config for this run only — no config file is modified. awman uses the agent-specific image (`awman-{project}-{agent}:latest`) for the session.

If the agent image does not yet exist, awman offers to download the template and build both the project base image (if needed) and the agent image before launching.

Interactive Codex sessions run as `codex --no-daemon`: recent Codex releases otherwise try to start a background server that the container doesn't have, and exit with `daemon executable not found`. If an older Codex image rejects the flag, rebuild it with `awman ready --build --no-cache` (without `--no-cache`, Docker reuses the cached layer holding the old Codex). Non-interactive runs (`codex exec`) don't get the flag, because they reject it. An `agents.flags` entry for `--no-daemon` is redundant: awman drops it from interactive launches and warns that non-interactive runs may reject it.

Passing an unknown agent name exits immediately with a list of valid options:

```
error: unknown agent 'foo'; first-party: claude, codex, opencode, gemini, copilot, cline, antigravity; custom: (none); define one with: awman new agent foo
```

### `--model <NAME>`

Override the model used by the launched agent for this session.

```sh
# CLI
awman chat --model claude-opus-4-6
awman exec workflow path/to/workflow.toml --model claude-haiku-4-5
awman chat --model=gpt-4o               # --flag=value form is also accepted

# TUI command box
chat --model claude-opus-4-6
exec workflow path/to/workflow.toml --model=claude-haiku-4-5
```

Both `--model NAME` and `--model=NAME` forms are accepted in both the CLI and the TUI command box.

The model name is passed verbatim to the agent's own model flag — awman does not validate the value. If the name is not recognised by the agent, the agent surfaces its own error. This means any model the agent supports can be used without awman needing updates when providers release new models.

Per-agent translation and expected `<NAME>` format:

| Agent | Flag appended | Expected format |
|-------|--------------|-----------------|
| `claude` | `--model <NAME>` | bare model ID (e.g. `claude-opus-4-6`) |
| `codex` | `--model <NAME>` | bare model ID (e.g. `gpt-4o`) |
| `gemini` | `--model <NAME>` | bare model ID (e.g. `gemini-2.0-flash`) |
| `antigravity` | `--model <NAME>` | model slug from `agy models` |
| `opencode` | `--model <NAME>` | **`provider/model` required** (e.g. `anthropic/claude-3-5-sonnet`) |
| `cline` | `--model <NAME>` | model ID for the selected provider; use `--agent-flag --provider` to select a provider for one run |
| `copilot` | `--model <NAME>` | bare model ID; cannot be applied under the `docker-sbx-experimental` runtime (use the `/model` slash command there) |

For agents that support multiple providers (`opencode`, for example), the `provider/model` slash form lets you target a specific provider when more than one is configured. awman passes the value through verbatim — the agent does the routing.

An agent with no model flag (a custom agent whose definition declares none) refuses `--model` with `agent '<name>' does not support a model flag`.

`--model` can be combined freely with `--agent`, `--yolo`, `--auto`, and all other flags. When used with `exec workflow`, the flag value acts as the default model for every workflow step that does not define its own `model` field. See [Per-step model overrides](05-workflows.md#per-step-model-overrides).

Under the `docker-sbx-experimental` runtime, the flag is delivered through the sandbox's per-launch session config (built-in template agents) or as launch arguments (custom-kit agents) rather than directly on the command line; the supported agents and modes are the same, except copilot, which cannot receive a model override there. See [Runtimes](11-runtimes.md#known-limitations).

### `--headless` / `-n`

Run the agent in print/batch mode — no interactivity required. The agent executes, produces output, and exits. `-n` is a short alias for `--headless` and works on every command that has the flag (`chat`, `exec prompt`, `exec workflow`, `ready`, `specs amend`, `new spec`, `new skill`). It also makes awman's own prompts take their non-interactive answer instead of asking, even on a terminal; `--json` implies it.

| Agent | Flag used |
|-------|-----------|
| Claude | `--print` |
| Codex | `exec` subcommand |
| OpenCode | `run` subcommand |
| Gemini | `--prompt` / `-p` |
| Antigravity | `--print` |
| Copilot | `-p <prompt>` |
| Cline | positional prompt (one-shot) |

Useful for CI pipelines, scripting, or when you want the output captured rather than live.

Under the `docker-sbx-experimental` runtime, agents that launch through Docker's built-in sandbox templates (`claude`, `codex`, `gemini`, `copilot`, `opencode`) cannot have their non-interactive flag enabled — awman warns at launch and pipes the prompt to the interactive entrypoint instead. See [Runtimes](11-runtimes.md#known-limitations).

### `--overlay <SPEC>`

Mount additional host resources into the agent container. Accepts typed overlay expressions:

- `dir(host_path:container_path[:ro|rw])` — mount a host directory (permission defaults to `:ro`)
- `env(VAR)` — pass a host environment variable into the container
- `skill(name)` / `skill(*)` — mount a named skill, pulled library, or all hand-authored global skills
- `ssh()` — mount `~/.ssh` read-only (for Git operations over SSH)

May be repeated or comma-separated. Available on `chat`, `exec prompt`, and `exec workflow`.

```sh
awman chat --overlay "env(ANTHROPIC_API_KEY)"
awman chat --overlay "ssh()"
awman chat --overlay "dir(/data/reference:/mnt/reference:ro)"
awman exec workflow path/to/workflow.toml --overlay "env(GITHUB_TOKEN),ssh(),skill(*)"
```

See [Overlays](08-overlays.md) for the full reference including config-based overlays, the `AWMAN_OVERLAYS` env var, and conflict resolution rules.
See [Security & Isolation](04-security-and-isolation.md#overlay-mounts) for security considerations.

### `--agent-flag <ARG>`

Append an extra argument to the agent's command for this run. Repeat it for multiple arguments; they are appended in order, after awman's own flags and before the initial prompt. Available on `chat`, `exec prompt`, and `exec workflow`. `remote exec` forwards it to the remote host, where it replaces that host's `agents.flags` entry for the run.

Because the value usually starts with `-`, write it in the `--agent-flag=<value>` form:

```sh
awman chat --agent codex --agent-flag=--search
awman exec prompt --agent-flag=--config --agent-flag=key=value "fix the build"
```

`--agent-flag` replaces the [`agents.flags`](07-configuration.md#pass-extra-flags-to-an-agent) config entry for the agent the command launches; it does not add to it. In `exec workflow` it applies to the workflow's default agent (the workflow's `agent` field, else `--agent`, else config); steps that name a different agent keep that agent's configured `agents.flags`. With `--dynamic`, it also reaches the leader when the leader's agent is the selected one (`--agent`, else config).

### `--allow-docker`

Mount the host Docker socket into the container, giving the agent the ability to build and run Docker containers. See [Security & Isolation](04-security-and-isolation.md#docker-socket-access) for details on when to use this.

### `--branch`, `--pr` and `--worktree`

`chat` and `exec prompt` normally run in your current checkout. Three mutually exclusive flags run the session in a separate Git worktree instead, so the agent works on a branch without touching your checkout. They are available on the CLI and in the TUI, not through the HTTP API.

```sh
awman chat --branch feature/login
awman exec prompt --pr 482 "Fix the failing test"
awman chat --worktree pr-482
```

Put these flags, like every other flag, before the prompt text. `exec prompt` reads everything after the prompt as more prompt, so `awman exec prompt "Fix the failing test" --pr 482` would send `--pr 482` to the agent and run in your checkout. awman refuses that order with an error rather than launching outside the worktree; quote the whole prompt as one argument if those words really belong in it.

Before launch awman prints the worktree path and branch. The session's container mounts that worktree (see [Security & Isolation](04-security-and-isolation.md#session-worktrees)).

**`--branch <name>`** runs in a worktree for that branch. What happens depends on where the branch exists:

| On `origin` | Local | Result |
|---|---|---|
| Yes | No | The branch is fetched and a worktree is created with a local branch tracking it |
| Yes | Yes | The local branch is fast-forwarded to `origin` when it can be, then used. If it has local commits that `origin` lacks, awman warns and keeps them |
| No | Yes | The existing local branch is used as it is |
| No | No | A new branch is created from the `HEAD` of the checkout you ran the command in. Uncommitted changes there are not carried over, and awman says so |

If there is no `origin` remote, awman warns and treats the branch as absent from the remote. A network or authentication failure while checking `origin` stops the command rather than quietly creating a new branch. awman never waits for a password or SSH prompt: SSH runs in batch mode (unless you set `GIT_SSH_COMMAND`, `GIT_SSH` or a global `core.sshCommand`), so connect to a new SSH host once outside awman to accept its host key, and use an SSH agent for keys with a passphrase. A remote that does not answer within five minutes is reported as an error. A remote-only branch in a single-branch clone gets a fetch mapping for that branch added to the remote, so its upstream works.

**`--pr <number|URL|owner/repo#N>`** runs in a worktree for a GitHub pull request. For a pull request from the same repository, the worktree uses the PR's head branch just like `--branch`. A pull request from a fork, or whose head branch was deleted, gets a local `pr/<N>` branch created from GitHub's pull ref; pushes from it do not update the PR. A closed or merged PR still works, with a warning. See [GitHub Integration](10-github-integration.md#pull-requests-for---pr) for accepted formats, authentication and fork details.

**`--worktree <name|path>`** reuses a worktree that already exists. A bare name is looked up under `~/.awman/worktrees/`; a value containing a path separator or starting with `.` is a path, and a relative path is resolved from the directory you ran the command in. A numeric name is also tried zero-padded to four digits (`7` also tries `0007`). The worktree must belong to this repository. Nothing is fetched, checked out or merged, and a worktree with a detached HEAD runs with a warning. If the name isn't found, the error lists the directories searched and the worktrees available.

Worktrees created by `--branch` and `--pr` live under `~/.awman/worktrees/<repo-name>-<hash>/`, in a directory named `br-<branch>` or `pr-<N>`. Characters in a branch name other than letters, digits, `.`, `_` and `-` become `-`, with a short hash appended so `a/b` and `a-b` never collide. Because of that hash, `--branch feature/login` creates a directory such as `br-feature-login-1a2b3c4d`, while `--branch fix-123` creates plain `br-fix-123`. Reopen one later with the directory name awman printed, for example `--worktree br-feature-login-1a2b3c4d` or `--worktree pr-482`; if you get the name wrong, the error lists the worktrees that exist.

Details that apply to all three:

- **Worktrees are kept.** awman never merges, discards or deletes these worktrees, and there is no prompt afterwards. When the session ends it prints `Worktree kept at <path> (<branch>).` Remove one yourself with `git worktree remove`.
- **Already checked out is reused.** If the branch is already checked out in another worktree of this repository, awman uses that one. If it is checked out in your main checkout, the command stops and tells you to switch that checkout to another branch or run without the flag.
- **Reused worktrees are refreshed carefully.** awman fast-forwards a reused worktree only if it is clean and strictly behind its remote; one with uncommitted changes, local commits or diverged history is left as it is, with a warning. This also applies when another session is already running in that worktree: if its tree is clean and behind, the second launch fast-forwards it underneath the first. Use `--worktree <name>` for the second session if you want to join it without any fetch or fast-forward.
- **A taken path is never overwritten.** If the default `br-`/`pr-` directory already exists for something else, the command stops and names it. A stale worktree registration also stops the command; run `git worktree prune` yourself after inspecting it.
- **Your checkout's configuration applies.** Agent, trust, repo config and overlays come from the checkout you ran the command in, not from the worktree. See [Security & Isolation](04-security-and-isolation.md#session-worktrees).
- The repository must have at least one commit.

`exec workflow` also has a `--worktree` flag, but there it is a switch that creates and manages a workflow worktree, implied by `--yolo` and `--auto`, with a merge-back prompt at the end; see [Workflows](05-workflows.md#worktrees).

---

## Permission modes

Every agent-launching command (`chat`, `exec prompt`, `exec workflow`, and their `remote` equivalents) runs at one of four permission levels. They differ only in how much the agent may do without asking you first:

| Level | Flag | The agent… |
|-------|------|-----------|
| **Ask** (default) | *(none)* | Prompts before every file edit and shell command |
| **Plan** | `--plan` | Reads and analyses only — cannot modify files at all |
| **Auto** | `--auto` | Auto-approves file edits and writes; still prompts before shell commands and other high-risk operations |
| **Yolo** | `--yolo` | Skips every permission prompt and never pauses for confirmation |

When both `--yolo` and `--auto` are passed, `--yolo` wins. `--plan` and `--yolo` together are refused (`plan and yolo modes are mutually exclusive`).

Not every agent implements every level. `--plan` on an agent with no plan mode is refused before launch, so a read-only request never runs with write access. For `--auto` and `--yolo`, awman simply omits the flag where an agent has no equivalent — the session still launches, but at the agent's own default permission level rather than the one you asked for. **Under the container runtimes this is silent**, so check the tables below before relying on a mode. (Under `docker-sbx-experimental` the unsupported combinations are warned about at launch — see [Runtimes](11-runtimes.md#known-limitations).)

### `--plan`

Start the agent in its planning mode. Each CLI enforces its own planning permissions; use this to explore an approach before implementation.

| Agent | Plan mode |
|-------|-----------|
| `claude` | `--permission-mode plan` |
| `codex` | *(no startup plan flag — refused; use `/plan` interactively)* |
| `opencode` | *(not supported — refused)* |
| `gemini` | `--approval-mode=plan` |
| `antigravity` | `--mode plan` |
| `copilot` | `--plan` |
| `cline` | `--plan` |

`--plan` can be combined with `--headless`.

### `--auto`

Enable the agent's intermediate automatic approval mode. The exact permissions vary by CLI: OpenCode approves actions that are not explicitly denied, Cline approves all tools with this flag, and Codex permits writes inside its workspace sandbox.

| Agent | `--auto` flag |
|-------|--------------|
| `claude` | `--permission-mode auto` |
| `codex` | `--sandbox workspace-write` |
| `opencode` | `--auto` |
| `gemini` | `--approval-mode=auto_edit` |
| `antigravity` | `--mode accept-edits` |
| `copilot` | *(no equivalent — flag omitted)* |
| `cline` | `--auto-approve true` |

Combined with `exec workflow`, `--auto` implies `--worktree`, but it does **not** auto-advance stuck steps — that countdown is `--yolo`-only.

### `--yolo`

Unattended workflow operation. awman applies the agent's permission bypass flag where one exists and advances workflow steps without waiting for approval.

```sh
awman exec workflow aspec/workflows/implement-feature.toml --yolo
awman chat --yolo
```

Yolo mode is a good fit when you have a well-specified work item you trust the agent to implement, when you want a multi-step workflow to run end-to-end without manual advancement, or when you have already reviewed the approach in a `--plan` session. It is a poor fit for work that will hit decisions genuinely needing your input, for open-ended `chat` sessions, and for anything difficult to undo.

`--yolo` does three things:

**1. Requests the agent's permission bypass mode where supported.** The agent-specific flag is appended to the container entrypoint before launch:

| Agent | Flag appended |
|-------|--------------|
| `claude` | `--dangerously-skip-permissions` |
| `codex` | `--dangerously-bypass-approvals-and-sandbox` |
| `opencode` | *(no equivalent — flag omitted)* |
| `gemini` | `--approval-mode=yolo` |
| `antigravity` | `--dangerously-skip-permissions` |
| `copilot` | `--yolo` (allow all permissions) |
| `cline` | `--yolo` |

**2. Implies `--worktree` for workflow execution.** Running a workflow with `--yolo` automatically creates an isolated Git worktree, and prints:

```
--yolo implies --worktree. Running in isolated worktree.
```

Passing `--worktree` as well is accepted — no duplicate worktree is created. This applies to `exec workflow` only: on `chat` and `exec prompt`, `--yolo` does not create a worktree. Use `--branch`, `--pr` or `--worktree <name>` there if you want isolation (see [`--branch`, `--pr` and `--worktree`](#--branch---pr-and---worktree)).

**3. Auto-advances stuck workflow steps.** When a step goes silent, a countdown advances the workflow for you. See [Auto-advance when stuck](05-workflows.md#auto-advance-when-stuck-yolo-mode) in the workflow guide for the countdown, how it appears in each frontend, and how to cancel it.

### Disallowed tools

`agents.yolo.disallowedTools` lists tools the agent may not use while `--yolo` is on. Set it in per-repo or global config (a repo list replaces the global one; lists are not merged):

```json
{
  "agents": { "yolo": { "disallowedTools": ["Bash", "computer"] } }
}
```

Every `--yolo` launch — `chat`, `exec prompt`, each `exec workflow` step, and API commands, which default to `--yolo` — passes the list to the agent. Without `--yolo` the list is ignored.

| Agent | How the list is applied |
|-------|-------------------------|
| `claude` | `--disallowedTools Bash,computer` (under Docker Sandboxes, its sandbox settings) |
| every other agent | No disallowed-tools mechanism: the `--yolo` launch is refused |

A tool restriction is never dropped silently. When the agent cannot enforce the list, awman refuses to start it:

```
invalid configuration: agents.yolo.disallowedTools (Bash, computer) is set, but agent 'codex' has no way to enforce a disallowed-tools list here, so awman refuses to launch it with --yolo. ...
```

To run such an agent with `--yolo` in one repo, clear the list there with `awman config set agents.yolo.disallowedTools ""` (an empty repo list replaces the global one), or run without `--yolo`. A custom agent applies the list when its definition declares a disallowed-tools flag.

### Yolo through the HTTP API

Commands submitted through the API (`POST /v1/worker/commands`) run with `--yolo` applied **by default** for `chat`, `exec prompt`, and `exec workflow` — the server assumes unattended execution unless your request specifies the flag itself. The response's `flags_applied` field confirms which defaults were used; see [Submit a command](09-api-and-remote-mode.md#submit-a-command).

### Security considerations

- `--yolo` removes the human checkpoints that catch unintended agent actions. Only use it with agents and work items you trust.
- `exec workflow --yolo` is the recommended autonomous pattern: isolated branch, structured phases, auto-advancing, easy to discard if the output isn't right.
- Gemini's `--yolo` skips all tool confirmations including shell commands; `--auto` (`--approval-mode=auto_edit`) is the more conservative choice.
- Copilot supports `--yolo` only — `--auto` has no equivalent and is dropped.
- Cline's `--auto` enables tool auto-approval; its `--yolo` skips confirmations and exits after the turn.

---

## Custom agents

Any CLI coding agent that awman does not ship with can be added as a **custom agent**: a small TOML definition in `.awman/agents/` (this repo) or `~/.awman/agents/` (all repos). Once defined and approved, it works with `--agent`, `awman config set agent`, and workflow steps like a built-in agent. Capabilities such as plan, auto and yolo modes, `--model`, and non-interactive mode come from the flags in the definition, and are silently skipped (or rejected, for `--plan`) when the definition does not declare them. See [Custom Agents](15-custom-agents.md) for the full guide, including `awman new agent`.

---

## Work item management

### Creating a work item

```sh
awman new spec
# or in TUI:
new spec
```

Prompts for a type (Feature, Bug, Task, or Enhancement) and a title, then creates a numbered work item file in the configured work items directory using the project's template. Answer with the digit shown next to your choice (`1`-`4`); the CLI and TUI accept exactly the same input here. Dismissing the question (Esc in the TUI, Ctrl-D on the CLI) aborts the interview rather than filing a Task by default.

By default, awman writes to `aspec/work-items/` and uses `aspec/work-items/0000-template.md`. If neither exists, awman auto-discovers any `*template.md` file in the work items directory and prompts you to confirm it. You can also configure the paths explicitly:

```sh
awman config set workItems.dir docs/work-items
awman config set workItems.template docs/work-items/my-template.md
```

If no template is found or confirmed, the new file is created with a minimal stub (`# Kind: Title`). See [Configuration: Work item paths](07-configuration.md#custom-work-item-paths) for full details on path resolution and auto-discovery.

```sh
awman new spec --interview
```

After creating the file, prompts for a brief summary of the work, then launches an agent session to complete the spec — filling in user stories, implementation plan, edge cases, and test plan based on your summary. More thorough specs lead to better implementations.

In the TUI, a freeform text box dialog opens for the summary input. Use **Ctrl+Enter** to submit or **Esc** to cancel.

### Creating a spec from a GitHub issue

```sh
awman new spec --issue 84                                                      # bare number
awman new spec --issue prettysmartdev/awman#84                                 # owner/repo shorthand
awman new spec --issue https://github.com/prettysmartdev/awman/issues/84      # full URL
```

Fetches the GitHub issue and launches an agent to generate a structured work item spec from its content. Combined with `--interview`, the issue description is pre-populated in the text box for editing before the agent runs.

For full details on GitHub integration, authentication, and input formats, see [GitHub Integration](10-github-integration.md).

### Updating a spec after implementation

```sh
awman specs amend 0001
```

After implementing a work item, the actual implementation sometimes differs from the original spec. `specs amend` launches the agent to review the code that was written and update the spec to match — adding an "Agent implementation notes" section describing what changed and why. Useful for keeping specs accurate as a long-term reference.

---

## Creating skills

Skills are reusable instruction files that teach an agent how to perform a specific task when invoked with `/skill-name`. `awman new skill` scaffolds one:

```sh
# CLI
awman new skill

# TUI command box
new skill
```

awman asks for:

1. **Skill name** — used as the directory name and slash-command trigger (e.g. `run-tests`). Use a short kebab-case slug.
2. **Body** — the skill's instruction text. The CLI reads a single line; the TUI opens a multi-line editor (**Enter** starts a new line, **Ctrl+Enter** or **Ctrl+S** submits, **Esc** cancels).

The file is written to `.claude/skills/<name>/SKILL.md` in the current repo, headed `# Skill: <name>` and followed by your body. An empty body leaves a skeleton with `Description` and `Body` headings to fill in. An existing `SKILL.md` at that path is overwritten.

Write the body in second-person imperative ("Run …", "Check …", "If … then …"):

```markdown
# Skill: run-tests

Run `make test` and wait for output.
If tests fail, show the failing test names and exit codes.
If all tests pass, confirm success and stop.
```

### Interview mode

```sh
awman new skill --interview
```

Instead of a body, describe what the skill should do; a code agent writes the skill for you, adding any commands, code examples, or decision trees it needs. The summary is multi-line: in the TUI, **Ctrl+Enter** submits; on the CLI, end it with a blank line or **Ctrl-D**.

The interview agent runs in a container with the new skill's own directory mounted at `/awman/skill`, and is told to edit `/awman/skill/SKILL.md` and nothing else. That is what lets `--interview` work with `--global`, whose skills live outside the repo the container sees at `/workspace`. Interview mode is not available under the `docker-sbx-experimental` runtime.

### Global skills

```sh
awman new skill --global
```

Writes to `~/.awman/skills/<name>/SKILL.md` instead of the current repo. Use this to maintain a personal library of skills that travel with you across projects.

`--global` and `--interview` can be combined: the agent's extra mount is only `~/.awman/skills/<name>/`, never your home directory. This still has to be run inside a Git repository, whose agent image the interview uses.

Global skills reach agent containers through the `skill()` overlay — `skill(*)` for all of them, `skill(<name>)` for one — in config or on the command line:

```json
{ "overlays": ["skill(*)"] }
```

```sh
awman exec workflow path/to/workflow.toml --overlay "skill(*)"
```

Once mounted, they appear as slash commands. See [Overlays](08-overlays.md).

### Pulled skill libraries

Your global skills store isn't limited to skills you write. Published libraries can be pulled into it from GitHub and curated alongside your own, so one collection covers both:

```sh
awman new skill --pull https://github.com/obra/superpowers
awman new skill --pull github.com/obra/superpowers
awman new skill --pull obra/superpowers
```

All three forms refer to the same repository. The library is stored at `~/.awman/skills/.library/superpowers/`; the final repository name is used as the library name. By default, awman looks for skill directories under the repository's `skills/` folder.

To refresh a library you have already pulled, use its short name:

```sh
awman new skill --pull superpowers
```

To refresh every pulled library:

```sh
awman new skill --pull-all
```

If the skills are in a different folder, provide a relative path inside the repository with `--subdir`:

```sh
awman new skill --pull github.com/example/team-skills --subdir .agents/skills
```

The selected subdirectory is remembered, so later `--pull team-skills` and `--pull-all` refreshes continue to use it. A successful pull reports the destination, the subdirectory, and the skills it found, in this form:

```
Pulled 'team-skills' into <library-directory> (3 skill(s) found under .agents/skills/): review, test, release
```

`--pull-all` continues refreshing the other libraries if one library fails and reports the number that succeeded and failed, for example `Skill library refresh complete: 2 succeeded, 1 failed.` Every reachable library is still refreshed, but the command exits non-zero when any library failed, so a scripted or CI refresh notices. When none have been pulled, it reports `no skill libraries pulled yet` and exits successfully. A short-name refresh for a library that has not been pulled yet reports that the library has not been pulled and directs you to use the full GitHub slug.

Pulled libraries are fetched, managed content. Refreshing one hard-resets its directory to the upstream version, so hand edits inside `~/.awman/skills/.library/` are discarded. Put personal skills in `~/.awman/skills/<name>/` instead. Pull operations are non-interactive, run entirely on the host as plain `git` commands (no container is launched), and do not create or edit a skill body.

awman only ever refreshes directories it created under `~/.awman/skills/.library/`, and never deletes or overwrites anything else. A pull is refused, with the offending path named, when:

- the target already holds something that is not an awman-managed clone (no `.git/` or no `.awman.json`);
- the target is a symlink rather than a real directory;
- the library was pulled from a different owner with the same repository name;
- the clone's git `origin` no longer matches the source recorded in its `.awman.json`.

In each case, remove the named directory yourself if you want to replace it.

Mount a pulled library with `skill(<library>)`, or one skill from it with `skill(<library>/<skill>)` — see [Overlays](08-overlays.md).

### Flags

| Flag | Description |
|------|-------------|
| `--interview` | Let a code agent complete the skill body from a short summary |
| `-n`, `--headless` | Run the interview agent in print mode |
| `--global` | Write to `~/.awman/skills/<name>/` instead of the current repo |
| `--pull <repo>` | Pull a GitHub skills library, or refresh one by its short name |
| `--pull-all` | Refresh every previously-pulled skills library |
| `--subdir <path>` | Use a different relative folder inside the pulled repository instead of `skills/` |

---

## Monitoring running agents

```sh
awman agents status             # one-shot snapshot
awman agents status --watch     # refresh the dashboard every 2 seconds
awman agents list               # list running agents
awman agents detail <id>        # inspect one agent
awman agents kill <id>          # confirm before stopping and removing
awman agents attach <id>        # attach to an agent's terminal
```

`agents status` shows a snapshot of running agents from the CLI or TUI. Add
`--watch` to keep refreshing the dashboard; in the TUI, press **Esc** to stop
the watch. `--json` emits a JSON snapshot and cannot be combined with
`--watch`.

`agents list` accepts the aliases `ps` and `ls`. Use `--all` to include
stopped agents that remain eligible for cleanup, `--no-trunc` to show full
IDs, names and image names, and `--json` for structured output. The `-q` / `--quiet`
flag prints only agent IDs and cannot be combined with `--json`; this is useful
for scripts that remove every listed agent:

```sh
awman agents ls -q | xargs -n1 awman agents rm -y
```

`agents detail <id>` (alias: `inspect`) shows identity, lifecycle, resource,
and runtime information for an agent. Supply its ID, a unique ID prefix, or
its name; use `--json` for structured output.

`agents kill <id>` (alias: `rm`) stops and removes an agent after confirmation.
Pass `-y` / `--yes` to skip the prompt, as in the scripting example above.
Use `--json --yes` to emit the kill result as JSON without a prompt.

Status and kill operations report errors from the configured runtime instead
of treating a failed listing as an empty result. For example, if Docker is
unavailable, `agents status` and `agents kill` surface the runtime failure
rather than reporting that no agent exists. Restore runtime availability and
retry.

`agents attach <id>` connects to the running agent's terminal. In the CLI,
press **Ctrl-C** to detach; this closes only your connection and leaves the
agent running. In the TUI the agent opens in the tab's container window, where
keys (Ctrl-C included) go to the agent; **Ctrl-\\** leaves the view without
touching it. Apple agents launched by older versions without launcher metadata
must be restarted before they can be attached. The API exposes
the `status` snapshot, `list`, `detail`, and `kill` commands; API `kill` calls
must include `--yes`. `attach` and `status --watch` are available only in the
CLI and TUI.

Status and list output include a source marker for each agent. Ordinary user
sessions are marked `session`; an agent your squad launched is marked
`squad:<task>`, such as `squad:issue-triage`. This marker identifies which task
owns the background evaluation or generated workflow. It can appear even if
you have never enabled squad; it does not indicate a problem with a regular
session. See [Squad](12-squad.md) for task management and unattended execution.

---

## Agent authentication

awman mounts supported on-disk credentials or passes the host environment variables you select with `env()`. Host keyring credentials are available only where awman explicitly stages them; see the per-agent paths below.

For Claude Code, awman reads your OAuth credential from the macOS Keychain (service: `Claude Code-credentials`) — or, on non-macOS hosts, from `~/.claude/.credentials.json` — and writes an awman-authored `.credentials.json` (access token, expiry, and scopes only) into the staged `~/.claude` directory that's mounted into the container. Your refresh token is never captured from the host credential and never mounted into a container.

While the session is running, a background monitor tracks the token's expiry and, before it runs out, pings your host's local Claude Code installation — the same sanctioned check `awman ready` uses — so it rotates its own Keychain entry. awman then atomically rewrites the staged credential file for every live session, so a long-running container picks up the new token on its next request with no restart. If the host can't refresh (asleep, logged out, offline), awman keeps the last-known-good token, warns you, and retries; a workflow step that fails on an auth error is retried once automatically after a fresh refresh. See [Control credential refresh](07-configuration.md#control-credential-refresh-agentsauthrefresh) to change the refresh threshold or turn this off.

The token value is never shown in displayed container commands; the mounted `~/.claude` directory appears only as an ordinary bind-mount path.

| Agent | Auth mechanism |
|-------|---------------|
| `claude` | OAuth credential read from macOS Keychain (or `~/.claude/.credentials.json`), delivered as a live-refreshed `.credentials.json` file in the staged `~/.claude` mount — never as an env var |
| `codex` | `~/.codex/` mounted directly when present (including file-backed `auth.json`); or `OPENAI_API_KEY` through `env()` |
| `opencode` | `~/.config/opencode/` config and `~/.local/share/opencode/` provider credentials mounted directly when present; or provider key through `env()` |
| `gemini` | API key via `env()` overlay and/or `~/.gemini/` OAuth directory (auto-mounted) |
| `antigravity` | Google account credential staged under `~/.gemini/antigravity-cli/`, or `GEMINI_API_KEY` through `env()` when `modelProvider` is `gemini` |
| `copilot` | `~/.copilot/` mounted directly when present (including `settings.json` and any file-backed token in `config.json`); or GitHub token via `env()` overlay (`COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN`) |
| `cline` | `~/.cline/data/` mounted directly when present (current provider settings are in `settings/providers.json`) |

`env()` overlays pass named host variables into the agent container. Host directories mounted directly are live and writable from the container; account credentials in those directories should be treated as secrets. Claude and Antigravity use staged copies.

### Host settings injection

For Claude sessions, awman also mounts sanitized copies of your Claude Code settings so the agent starts pre-configured with your model preferences, plugins, and onboarding state:

| Host file | Container path | Notes |
|-----------|----------------|-------|
| `~/.claude.json` | `$HOME/.claude.json` | `oauthAccount` field stripped to prevent broken auth state |
| `~/.claude/settings.json` | `$HOME/.claude/settings.json` | Model preferences and plugins in the staged settings directory |

Your original files are never modified. The copies are created in a temporary directory before each launch and cleaned up when the container exits.

---

## Gemini authentication

Gemini supports two authentication paths. You can use either or both — awman sets up both automatically.

### API key (`env()` overlay)

Add `GEMINI_API_KEY` (or one of the Vertex AI variables) to your overlays config:

```json
{ "overlays": ["env(GEMINI_API_KEY)"] }
```

Or pass it per-command: `awman chat --agent gemini --overlay "env(GEMINI_API_KEY)"`.

Get an API key from [Google AI Studio](https://aistudio.google.com/apikey). awman reads the value from your host shell and injects it into the container. The value is masked (`***`) in displayed Docker commands.

Supported Gemini auth environment variables:

| Variable | Description |
|----------|-------------|
| `GEMINI_API_KEY` | API key from Google AI Studio |
| `GOOGLE_API_KEY` | Alternate Gemini API key variable; select the appropriate auth method in Gemini CLI |
| `GOOGLE_CLOUD_PROJECT` | Vertex AI project ID |
| `GOOGLE_CLOUD_LOCATION` | Vertex AI region |
| `GOOGLE_GENAI_USE_VERTEXAI` | Set to `true` to enable the Vertex AI auth path |

> **Note on `GOOGLE_APPLICATION_CREDENTIALS`:** This variable points to a file path on the host. Passing it via `env()` overlay injects the path string but not the file itself, so the container cannot read it. Service account JSON authentication requires either embedding the key in your `Dockerfile.dev` or mounting it with a `dir()` overlay. For most users, `GEMINI_API_KEY` is simpler.

### OAuth token (`~/.gemini/` mount)

Gemini's Google sign-in caches its credential under `~/.gemini/` (typically `oauth_creds.json`); `settings.json` holds configuration, not the token. When the host directory exists, awman mounts it at the container user's `$HOME/.gemini`.

If `~/.gemini/` does not exist on the host, awman does not mount it. Gemini will need an API key or an interactive sign-in.

The mount is a live bind mount. Gemini can update the host's credential cache while a session runs.

### Auth precedence

The selected Gemini CLI authentication method determines which credential is used. awman does not choose between a key and Google sign-in.

---

## Antigravity authentication

Antigravity supports Google account sign-in and direct Gemini API access. Direct API access requires a matching setting and key.

### API key (`env()` overlay)

Set `"modelProvider": "gemini"` in `~/.gemini/antigravity-cli/settings.json`, then add `GEMINI_API_KEY` to your overlays config:

```json
{ "overlays": ["env(GEMINI_API_KEY)"] }
```

Or pass it per-command: `awman chat --agent antigravity --overlay "env(GEMINI_API_KEY)"`.

Get an API key from [Google AI Studio](https://aistudio.google.com/apikey). awman reads the value from your host shell and injects it into the container. The value is masked (`***`) in displayed Docker commands. `GOOGLE_API_KEY` and `ANTIGRAVITY_API_KEY` are not recognized by this auth path.

If you remove `GEMINI_API_KEY`, also remove `modelProvider: "gemini"`; otherwise `agy` exits at startup.

### OAuth token (`~/.gemini/antigravity-cli/` mount)

`settings.json` holds preferences, not the OAuth token. awman stages a copy of the host's `~/.gemini/` directory and seeds `antigravity-cli/antigravity-oauth-token` from the host credential store when available. It mounts the staged directory at the container user's `$HOME/.gemini`.

If neither host settings nor a usable host credential exists, there is no staged auth mount. Sign in with `agy` on the host first or configure the Gemini API key path above.

Changes inside the staged copy do not modify the host directory.

### Auth precedence

Antigravity uses the Gemini API key only when `modelProvider` is `gemini`. Without that setting it uses account sign-in.

### Model configuration

Use awman's `--model <slug>` for one session; `agy models` lists available slugs. For a persistent choice, use `/model` in `agy` or edit `~/.gemini/antigravity-cli/settings.json`.

---

## Gemini deprecation notice

The `gemini` agent is deprecated by Google in favor of Antigravity. When you launch a `gemini` session using `awman chat --agent gemini` or set `agent = "gemini"` in your config, a deprecation warning appears before the container starts:

```
The 'gemini' agent is deprecated by Google. Migrate to 'antigravity' — run 'awman chat --agent antigravity' (or 'awman config set agent antigravity' to change your default).
```

The warning does not block execution — your gemini session still starts. However, you should plan to migrate to `antigravity`:

1. Try it once — `awman chat --agent antigravity` automatically downloads `Dockerfile.antigravity` and builds the agent image on first use.
2. Make it your default: `awman config set agent antigravity` (add `--global` to apply across all repos).
3. Set up authentication as described in [Antigravity authentication](#antigravity-authentication) above.

Antigravity uses a separate `agy` CLI and its own settings and authentication paths.

---

## Copilot authentication

awman mounts `~/.copilot/` when it exists, carrying `settings.json` and any token Copilot stored in `config.json` on a host without a usable keyring. Credentials in the host keyring are not available inside the container. You can also pass a token through an environment overlay:

```json
{ "overlays": ["env(COPILOT_GITHUB_TOKEN)"] }
```

Or pass it per-command: `awman chat --agent copilot --overlay "env(COPILOT_GITHUB_TOKEN)"`.

Copilot reads the following environment variables in precedence order:

| Variable | Description |
|----------|-------------|
| `COPILOT_GITHUB_TOKEN` | Dedicated Copilot token (highest precedence) |
| `GH_TOKEN` | Standard GitHub CLI token |
| `GITHUB_TOKEN` | Fallback GitHub token |
| `COPILOT_GH_HOST` | GitHub Enterprise hostname override |

The token must have the "Copilot Requests" fine-grained PAT permission, or be a standard GitHub OAuth token obtained via `gh auth token`. Values are masked (`***`) in all displayed Docker commands.

For GitHub Enterprise users, add `COPILOT_GH_HOST` alongside the token:

```json
{ "overlays": ["env(COPILOT_GITHUB_TOKEN)", "env(COPILOT_GH_HOST)"] }
```

---

## Custom agents and provider API keys

Agents that authenticate with provider API keys — Crush and Maki, for example, now shipped as [custom agent](15-custom-agents.md) examples — receive those keys from your host environment. Either list the variable names under `[auth] env` in the agent's definition, or pass them with `env()` overlays:

```json
{ "overlays": ["env(ANTHROPIC_API_KEY)"] }
```

Only variables present in your host shell are injected, and values are masked (`***`) in all displayed Docker commands. See the Crush example (`awman new agent crush --from crush`) and [Migrating from crush and maki](15-custom-agents.md#migrating-from-crush-and-maki).

---

## Cline authentication

Cline's current CLI stores provider configuration and keys in `~/.cline/data/settings/providers.json`; older installations may still have `secrets.json` for migration. When `~/.cline/data/` exists, awman mounts it directly into the container at `$HOME/.cline/data`, so `cline auth` does not need to run again in every container.

No overlay configuration is needed — credentials travel with the auto-mounted directory.

If `~/.cline/data/` does not exist on the host, awman skips the mount. Configure Cline on the host with `cline auth` first, or authenticate inside an interactive session.

The mount is a live bind mount. Cline can update host credentials and session state while it runs.

To set up credentials on the host before running awman:

```sh
# Authenticate with Anthropic (example)
cline auth -p anthropic -k <your-api-key> -m claude-sonnet-5-5

# Verify credentials were written
test -f ~/.cline/data/settings/providers.json
```


## Reference: `awman init`

```sh
awman init [--agent=<name>] [--aspec]
```

Initialises the current Git repository for use with awman. See [Getting Started](00-getting-started.md) for a full walkthrough.

| Flag | Values | Default |
|------|--------|---------|
| `--agent` | `claude`, `codex`, `opencode`, `gemini`, `antigravity`, `copilot`, `cline`, or a [custom agent](15-custom-agents.md) name | `claude` |
| `--aspec` | (flag) | off |

`--aspec` downloads the `aspec/` folder from `github.com/prettysmartdev/aspec`, providing spec templates and work item scaffolding. Skipped without the flag.

When `--aspec` is not passed and no `aspec/` folder exists, `init` offers to configure a custom work items directory and template path interactively. This sets `workItems.dir` (and optionally `workItems.template`) in the repo config so commands like `new spec` and `exec workflow` work without requiring the `aspec/` folder layout. See [Work item paths](07-configuration.md#custom-work-item-paths).

---

## Reference: `awman ready`

```sh
awman ready [--refresh] [--build] [--no-cache] [-n|--headless] [--allow-docker] [--json] [--agent <name>]
```

Verifies your environment is ready for agent sessions. Progress lines read `ready: <phase>` on stderr (suppressed with `--json`). A Docker or Apple Containers runtime error exits with status 3; every other engine failure, including builtin and image-builder errors, exits with status 1, and the CLI prints the same message the TUI and API show.

| Flag | Description |
|------|-------------|
| `--refresh` | Run the Dockerfile agent audit, which updates `Dockerfile.dev` for your project's toolchain (skipped by default) |
| `--build` | Rebuild the project base image and every agent image with a `.awman/Dockerfile.<agent>` (on the [builtin runtime](11-runtimes.md), re-acquire them from their image source) |
| `--no-cache` | Skip the build cache for any build `ready` runs; on the builtin runtime, also force a fresh image import |
| `--headless` / `-n` | Run the audit agent in print mode, and answer `ready`'s own questions non-interactively |
| `--allow-docker` | Give the audit container access to the host Docker socket |
| `--json` | Emit machine-readable JSON instead of the summary table. Implies `--headless`. See [`ready --json`](#ready---json) |
| `--agent <name>` | Check and build this agent instead of the configured one. For a [custom agent](15-custom-agents.md) whose definition needs approval, this is where you review and approve it |

Use `--refresh` after your project's toolchain changes. The audit only edits `Dockerfile.dev`; agent Dockerfiles are not touched.

### Other agents' images

`ready` checks the default agent's image and, for every other agent with a `.awman/Dockerfile.<agent>`, whether its image exists. Missing ones are listed on a `Missing images` row (a warning, not a failure) with a hint naming what `ready --build` would do for each. `ready --build` builds them all, one after another; a failed build for a non-default agent is reported but does not change `ready`'s exit status.

The summary also warns (without failing) when the repo has no `aspec/` folder or no `.awman/config.json`; run `awman init` to create them.

### Credential health

`awman ready` also reports the health of any live-refreshed agent credential (currently Claude's Keychain-backed OAuth token), as read on the host at the time you ran `ready`:

```
Credential claude (68432s remaining)   ✓
```

If the credential can't be read, has already expired, or its expiry can't be determined, the row shows a warning instead (`credential unreadable: ...`, `credential expired`, or `credential expiry unknown`) rather than failing the whole `ready` check.

### `ready --json`

With `--json`, `awman ready` prints one JSON object on stdout instead of the summary table — useful for CI and scripts:

```json
{
  "ready": true,
  "runtime": "docker",
  "agent_credentials": [
    { "agent": "claude", "refreshable": true, "expires_in_secs": 68432, "expired": false, "read_error": null }
  ],
  "steps": {
    "docker_daemon":     { "status": "ok",      "message": "" },
    "dockerfile":        { "status": "ok",      "message": "" },
    "aspec_folder":      { "status": "skipped", "message": "" },
    "work_items_config": { "status": "skipped", "message": "" },
    "local_agent":       { "status": "ok",      "message": "" },
    "dev_image":         { "status": "ok",      "message": "" },
    "refresh":           { "status": "skipped", "message": "" },
    "image_rebuild":     { "status": "skipped", "message": "" }
  }
}
```

Each step's `status` is `ok`, `skipped`, `warn`, `failed`, `pending` or `running`, with the reason in `message`. `refresh` is `ok` when `--refresh` ran the audit. On the builtin runtime the object also carries `steps.image_source` and, when images were built or imported, `image_results`, `base_image_results` and `warnings`.

`--json` implies `--headless`, so no prompts are shown, and audit output is not streamed. In `agent_credentials` (one entry per agent with a live-refreshed credential), `expires_in_secs` is `null` when the expiry couldn't be determined, and `read_error` is set when the credential couldn't be read at all. No token value ever appears in this output.

---

## Reference: all `chat` and `exec` flags

| Flag | `chat` | `exec prompt` | `exec workflow` | Description |
|------|--------|---------------|-----------------|-------------|
| `--agent=<name>` | ✓ | ✓ | ✓ | Override the agent for this session |
| `--model=<NAME>` | ✓ | ✓ | ✓ | Override the model used by the agent |
| `--headless` / `-n` | ✓ | ✓ | ✓ | Print/batch mode |
| `--set agents.launchMode=<stdio\|acp>` | ✓ | ✓ | ✓ | Choose standard stdio or ACP mode |
| `--plan` | ✓ | ✓ | ✓ | Read-only analysis mode |
| `--allow-docker` | ✓ | ✓ | ✓ | Mount host Docker socket |
| `--overlay=<SPEC>` | ✓ | ✓ | ✓ | `dir()`, `env()`, `skill()`, `ssh()` overlays (repeatable) |
| `--agent-flag=<ARG>` | ✓ | ✓ | ✓ | Extra argument for the agent command (repeatable); replaces its `agents.flags` entry |
| `--branch <NAME>` | ✓ | ✓ | — | Run in a worktree for this branch (CLI/TUI only) |
| `--pr <N\|URL\|owner/repo#N>` | ✓ | ✓ | — | Run in a worktree for a GitHub pull request (CLI/TUI only) |
| `--worktree <NAME\|PATH>` | ✓ | ✓ | — | Reuse an existing worktree (CLI/TUI only) |
| `--worktree` | — | — | ✓ | Run in an isolated, managed Git worktree (a switch on this command) |
| `--auto` | ✓ | ✓ | ✓ | Auto-approve file edits, prompt for shell commands |
| `--yolo` | ✓ | ✓ | ✓ | Fully autonomous mode |
| `--work-item <N>` | — | — | ✓ | Work item number for template variable substitution |
| `--issue <N\|URL>` | — | ✓ | ✓ | Use a GitHub issue as the prompt / work item input — see [GitHub Integration](10-github-integration.md) |
| `--dynamic` | — | — | ✓ | Have a leader agent design the workflow; implies `--yolo`, `--worktree`, `context(workflow)` |
| `--leader <agent::model>` | — | — | ✓ | Leader agent and model for `--dynamic` |
| `--on-existing-worktree`, `--on-dirty-merge-target`, `--on-merge-failure` | — | — | ✓ | Answer worktree lifecycle questions up front — see [Workflows](05-workflows.md#worktrees) |
| `--prerequisite-*`, `--check-prerequisites` | — | — | ✓ | Grant or check what setup/teardown steps need — see [Workflows](05-workflows.md) |
| `--json` | — | — | ✓ | Print the workflow outcome and prerequisite report as JSON |

The [Command Reference](14-command-reference.md) lists every flag with its exact values.

---

[← Using the TUI](02-using-the-tui.md) · [Next: Security & Isolation →](04-security-and-isolation.md)
