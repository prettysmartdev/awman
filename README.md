<p align="center">
  <img src="docs/awman_logo.svg" width="240" alt="awman logo">
</p>

# awman

[![CI tests](https://github.com/prettysmartdev/awman/actions/workflows/test.yml/badge.svg)](https://github.com/prettysmartdev/awman/actions/workflows/test.yml)

**Custom agent workflows for your hardest tasks, run safely in isolation.**

awman is an "Agent Workflow Manager" that designs custom agent workflows and runs them on your laptop, server, cloud VM, or homelab. Give it a work item: a dynamic workflow is designed by a leader agent and then executed end-to-end inside isolated containers or microVMs. One run gets you from a spec to a reviewed and merge-able PR. Supports any code agent you prefer including Claude Code, Codex, OpenCode, and more.

awman is the developer tool for code agent power users who need to build high-quality software faster.

![Parallel agent workflow in the awman TUI](docs/blog/images/dynamic-workflows.png)

## Let the task define the workflow

- **Let an agent build the workflow, or build your own.** [Dynamic mode](docs/06-dynamic-workflows.md) creates a custom workflow for a specific work item; [TOML or YAML workflows](docs/05-workflows.md) lets you design workflows yourself. awman validates either before execution.
- **Continuous progress, automatically.** Workflow execution automatically advances after successful steps. Steps can run in parallel when independent. You can use different agents and models for implementation and adverserial review.
- **Context follows the work.** Dynamic workflows share plans, decisions, and intermediate results between steps. Add [repo or global context](docs/08-overlays.md#context-overlays-in-depth) when knowledge should carry into future runs.
- **Failures don't erase progress.** Problems are always recoverable. Retry a step, step back, or resume later from a named step instead of replaying the whole task.

## Keep the run contained and reviewable

- **Isolated by default.** Agents see the mounted project; [overlays](docs/08-overlays.md) grant extra directories, skills, or environment variables only where needed. Dynamic runs use a separate Git worktree, so the change is easy to inspect.
- **Checks on the way to PR.** Workflow steps can run tests, commit, push, open a PR, and fix CI when it breaks.
- **Routine work, off your plate.** [Squad](docs/12-squad.md) can watch for new issues or failing PRs and launch workflows on a schedule. The [daemon API](docs/09-api-and-remote-mode.md) supports scripts and remote workers.

For exploratory work, launch Claude Code, Codex, OpenCode, Gemini, Copilot, Cline, Antigravity, or a [custom agent](docs/15-custom-agents.md) in a live session. The TUI keeps parallel sessions and their status in one place.

## Get started

You'll need a Git repository, an account for your chosen agent, and a supported runtime such as Docker, Apple Containers, Docker sbx, or the builtin microVM runtime: [supported runtimes](docs/11-runtimes.md).

```sh
curl -s https://prettysmart.dev/install/awman.sh | sh
cd your-repo
awman init --agent codex
awman ready
awman new spec --interview
```

`init` prepares the project; `ready` checks the runtime and agent authentication. `new spec --interview` turns your task summary into a numbered work item. Use the number it prints to run your first dynamic workflow:

```sh
awman exec workflow --dynamic --work-item 0042
```

Replace `0042` with your work-item number. awman generates the workflow and runs it autonomously in a separate Git worktree.

## Other ways to work

| Goal | Command |
|---|---|
| [Define the steps yourself](docs/05-workflows.md) | `awman exec workflow path/to/workflow.toml --issue 84 --worktree` |
| [Run a one-off prompt](docs/03-agent-sessions.md) | `awman exec prompt "Review this change for bugs"` |
| [Work directly with an agent](docs/03-agent-sessions.md) | `awman chat` |
| [Turn a GitHub issue into a work item](docs/10-github-integration.md) | `awman new spec --issue 84` |
| [Set up recurring squad work](docs/12-squad.md) | `awman squad add --interview` |
| [Watch and manage live sessions](docs/02-using-the-tui.md) | `awman` |
| [Serve work through the API](docs/09-api-and-remote-mode.md#running-the-api-server) | `awman config set daemon.features.worker.enabled true`<br>`awman config set daemon.features.worker.push.workDirs.allow "$PWD"`<br>`awman daemon start --background` |
| [Pull jobs from a work server](docs/17-workers-and-work-servers.md#link-a-work-server) | `awman link https://work.example.com --name team` |

## Documentation

[Getting started](docs/00-getting-started.md) · [Security and isolation](docs/04-security-and-isolation.md) · [Web app](docs/20-webapp.md) · [Command reference](docs/14-command-reference.md) · [License](LICENSE)
