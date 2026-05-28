<p align="center">
  <strong>Run and coordinate AI code agents from your terminal.</strong> <br>
  Parallel sessions, multi-step workflows, full container isolation.<br>
  <br>
  <img src="./docs/awman_logo.svg" width="620" alt="awman">
</p>

<p align="center">
  <img src="https://github.com/prettysmartdev/awman/actions/workflows/test.yml/badge.svg">
</p>

---

`awman` (Agentic Workflow Manager) is a developer tool that adds structure and automation to the entire agentic software development lifecycle, from issue to merged PR. Run multiple agent sessions in parallel, execute structured multi-step workflows, run agents on a fleet of remote machines, and keep everything safe — every agent runs inside a container, never on the host.

![awman TUI](./docs/blog/images/tui-screenshot.png)

---

## Installation

```sh
curl -s https://prettysmart.dev/install/awman.sh | sh
```

The installer detects your platform and puts `awman` on your `PATH`.

<details>
<summary>Other installation options</summary>

**With mise** — using the [GitHub backend](https://mise.jdx.dev/dev-tools/backends/github.html):

```sh
mise use -g github:prettysmartdev/awman
```

To pin to a specific version: `mise use -g github:prettysmartdev/awman@0.8.0`

**From GitHub Releases** — download the binary for your platform from [GitHub Releases](https://github.com/prettysmartdev/awman/releases):

| Platform | Asset |
|----------|-------|
| Linux (x86_64) | `awman-linux-amd64` |
| Linux (ARM64) | `awman-linux-arm64` |
| macOS (Intel) | `awman-macos-amd64` |
| macOS (Apple Silicon) | `awman-macos-arm64` |
| Windows (x86_64) | `awman-windows-amd64.exe` |

**From source** — requires Rust 1.94+ and make:

```sh
git clone https://github.com/prettysmartdev/awman.git
cd awman
sudo make install
```

</details>

> **Upgrading from amux?** The tool was previously named `amux`. After installing `awman`, remove any old `amux` binary, symlinks, or shell aliases — invoking `amux` will no longer work. Existing `~/.amux/` and `<git-root>/.amux/` directories are migrated automatically on first run; any `AMUX_*` environment variables you have set will print a one-time deprecation warning naming their `AWMAN_*` replacement (the old names are ignored).

---

## Quick Start

```sh
# 1. Initialize your repo (once per project)
awman init

# 2. Open the TUI
awman

# 3. Start an agent session
chat

# 4. Run the Dockerfile.dev refresh agent to
#    ensure all your project's tools get installed
ready --refresh
```

See the [Getting Started Guide](docs/00-getting-started.md) for a full walkthrough.

---

## What you can do

### Run multiple agents at once

From the `awman` TUI, Open new tabs with **Ctrl+T**. Each tab is independent — its own working directory, its own container, running in the background while you work in another tab. Switch between tabs with **Ctrl+A** / **Ctrl+D**.

If a running agent gets stuck or completes its task, its tab turns yellow so you know to check in.

### Run structured workflows

![awman workflows](./docs/blog/images/tui-workflow.png)

A workflow breaks complex work into phases — for example, plan → implement → review → docs. Each phase is a separate agent session. You review the output between phases and decide whether to continue, retry, or redirect.

Workflows are plain Markdown (or TOML, or YAML) files in your repo:

```markdown
## Step: plan
Prompt: Read work item {{work_item_content}} and produce an implementation plan.

## Step: implement
Depends-on: plan
Prompt: Implement work item {{work_item_number}} according to the plan.

## Step: review
Depends-on: implement
Prompt: Review the implementation for correctness and style.
```

```sh
awman exec workflow ./aspec/workflows/implement-feature.md --work-item 0027
```
Workflows can optionally be passed a specific work item - a spec you've written - to work on new features, fix bugs, etc.


### Use different agents per step

Each workflow step can specify which agent runs it:

```markdown
## Step: implement
Depends-on: plan
Agent: codex
Prompt: Implement the plan.

## Step: review
Depends-on: implement
Agent: claude
Prompt: Review for correctness and style.
```

Supported agents: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`. Steps without an `Agent:` field use your configured default.


### Hand off to the agent workflow completely (yolo mode)

![awman yolo mode](./docs/blog/images/tui-yolo-mode.png)

`--yolo` disables your agent's permission prompts and auto-advances completed workflow steps. Use it when you have a well-specified task and want to return to a finished result.

```sh
# Implement fully autonomously, changes isolated to a git worktree
awman exec workflow ./aspec/workflows/implement-feature.md --yolo --work-item 0042
```

When a workflow step completes, a 60-second yolo countdown starts. If the agent doesn't resume, the workflow advances automatically. The countdown is visible in the tab bar across all tabs — you can monitor multiple autonomous sessions without switching to each one.

`--yolo` with `exec workflow` automatically runs in an isolated Git worktree, so you can review and discard the result if it isn't right.

For lighter autonomy, `--auto` approves file edits automatically but still requires permission for other commands.

### Manage agents across remote machines

`awman api start` runs an HTTP server that allows remote control of awman. This is useful when you want to run heavy agent workflows on a remote machine or manage a fleet of agent-runner boxes.

```sh
# On the remote machine, start the API server (prints an API key on first run)
awman api start --port 9090
```

From your local machine, use `awman remote` or cURL:

```sh
awman config set remote.defaultAPIKey <key>
awman config set remote.defaultAddr <host>
awman remote session start /workspace/myproject
awman remote run "exec workflow aspec/workflows/implement-feature.md --work-item 0027" --session <id> --follow
```

```sh
# Create a session bound to a directory
curl -s -X POST http://localhost:9090/v1/sessions \
  -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  -d '{"workdir": "/workspace/myproject"}'

# Submit a command to that session
curl -s -X POST http://localhost:9090/v1/commands \
  -H "Authorization: Bearer <key>" \
  -H "x-awman-session: <session-id>" \
  -H "Content-Type: application/json" \
  -d '{"subcommand": "exec", "args": ["workflow", "aspec/workflows/implement-feature.md", "--work-item", "0027"]}'

# Poll for completion, then fetch the log
curl -s http://localhost:9090/v1/commands/<command-id>
curl -s http://localhost:9090/v1/commands/<command-id>/logs
```
API commands run inside containers with the same isolation as running awman locally. All inputs and outputs and logs are stored in `~/.awman/api/` on the server for later review or auditing. The API server is authenticated using an API key generated the first time it is run, and can be refreshed (invalidating the old key) using `awman api start --refresh-key`.

See [API Mode](docs/08-api-mode.md) and [Remote Mode](docs/09-remote-mode.md) for details.

---

## Security

Every agent runs inside a Docker container built from `Dockerfile.dev` — agent-generated code never executes on your host machine.

- Only the current Git repository is mounted into the container
- Credentials are passed as environment variables and masked in all displayed commands — never written to files inside containers
- `awman ready --refresh` scans your project and updates `Dockerfile.dev` with exactly the tools your workflow needs
- awman itself is a statically compiled Rust binary — it cannot be modified by anything running inside a container

Apple Containers (macOS 26+) is also supported as an alternative to Docker Desktop.

![awman TUI status](./docs/blog/images/tui-status.png)

---

## Commands

```sh
awman                                  # open the TUI
awman init [--agent <name>]            # set up a project
awman ready [--refresh]                # verify environment; rebuild Dockerfile.dev
awman chat [--agent <name>] [--plan] [--auto] [--yolo]
awman exec prompt "<prompt>"           # run a one-off prompt in a container
awman exec workflow <path> [--work-item <nnnn>] [--yolo] [--worktree]
awman new spec [--interview]           # create a work item
awman new workflow [--interview]       # create a workflow file
awman new skill [--interview]          # create a skill file
awman specs amend <nnnn>               # update a spec to match what was built
awman status [--watch]                 # dashboard of all running agent containers
awman config show                      # view all config values
awman api start [--port <n>]           # start the HTTP API server (generates API key on first run)
awman api status                       # check if the API server is running
awman api kill                         # stop the API server
awman remote run <cmd> [--follow]      # run a command on a remote API server
awman remote session start <dir>       # create a session on a remote server
awman remote session kill <id>         # close a session on a remote server
```

All commands work in both TUI mode (without the `awman` prefix) and CLI mode.

---

## Documentation

- [Getting Started](docs/00-getting-started.md)
- [Using the TUI](docs/01-using-the-tui.md)
- [Agent Sessions](docs/02-agent-sessions.md)
- [Security & Isolation](docs/03-security-and-isolation.md)
- [Workflows](docs/04-workflows.md)
- [Yolo Mode](docs/05-yolo-mode.md)
- [Configuration](docs/07-configuration.md)
- [API Mode](docs/08-api-mode.md)
- [Remote Mode](docs/09-remote-mode.md)
- [Architecture](docs/architecture.md)

---

## License

See [LICENSE](LICENSE) for details.
