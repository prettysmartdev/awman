<p align="center">
  <img src="docs/awman_logo.svg" width="440" alt="awman logo">
  <br>
  [![CI tests](https://github.com/prettysmartdev/awman/actions/workflows/test.yml/badge.svg)](https://github.com/prettysmartdev/awman/actions/workflows/test.yml)
</p>

# awman - Agent Workflow Manager

**Custom agent workflows for your hardest tasks, run safely in isolation.**

`awman` creates custom, repeatable agent workflows to encode best practices like clear specs, task breakdown, independent review, enforced tests, and autoremediation into one developer tool.

Give awman any task:
- The `awman` leader agent designs a custom workflow.
- The workflow is validated and executed in isolated containers or microVMs.
- Shared context system carries progress and decisions between agent steps.
- Every run finishes with a PR and passing CI.
- Runs on your laptop, server, cluster, or homelab.
- Combine Claude Code, Codex, OpenCode, or any preferred code agent with your own subscriptions.

![Parallel agent workflow in the awman TUI](docs/blog/images/dynamic-workflows.png)

## Make quality repeatable

- **A bespoke workflow for each task:** Your work item sets the goal, and [dynamic mode](docs/06-dynamic-workflows.md) designs a multi-agent workflow for it. You can also [define it yourself](docs/05-workflows.md). `awman` validates every workflow before running each agent in isolated environments.
- **Mix and match agents for better results:** Put implementation, review, and verification in separate steps, with different agents or models you choose. The change gets challenged before it reaches your PR.
- **Keep decisions in the loop:** `awman` workflows pass plans, notes, and intermediate results through shared [workflow context](docs/08-overlays.md#workflow-context). Later agents can build on earlier findings instead of rediscovering them.
- **Fix issues automatically.** Persistent workflow state lets you retry, step back, or resume when things go sideways. Use [`on_failure` agents](docs/05-workflows.md#step-remediation-with-on_failure) to automatically diagnose and fix a failed test or CI check and retry it.

## Keep each task contained and reviewable

- **Isolation lets you stay hands off, safely:** Agents run in microVMs or containers and can only access the mounted project. Optional [overlays](docs/08-overlays.md) grant extra directories, skills, or environment variables only where needed. Workflows execute in an isolated git corktree or fresh clone.
- **Continuous checks so everything stays green:** Workflow steps can run tests, commit and push changes, open a PR, and wait for CI results. Failing checks can automatically trigger an `on_failure` agent.
- **Delegate routine work to your squad:** The built-in [squad](docs/12-squad.md) can watch for any real world condition like new issues or failing PRs, and launch dynamic workflows automatically.
- **Fan out and get more done:** The [`awman` API](docs/09-api-and-remote-mode.md) supports scripts, webhooks, and remote worker setups on any server or cloud.

## One engine, five ways to use it

Use `awman` in whichever way makes sense to you: the [TUI](docs/02-using-the-tui.md) for live interaction, the [CLI](docs/14-command-reference.md) for scripting and CI, the [API](docs/09-api-and-remote-mode.md) for integrations, the [web app](docs/20-webapp.md) for point-and-click simplicity, or a [worker](docs/17-workers-and-work-servers.md) to pull jobs from anywhere. Each path uses the same workflow engine and isolated agent execution.

## Get started

You'll need a project in a git repo, an account for your chosen agent(s), and a supported runtime such as Docker, Apple Containers, Docker sbx, or the builtin microVM runtime: [supported runtimes](docs/11-runtimes.md).

```sh
curl -s https://prettysmart.dev/install/awman.sh | sh
cd your-repo
awman init --agent codex
awman # open the TUI
```

Some commands: `init` prepares the project; `ready` checks the runtime and agent authentication. `new spec --interview` turns your task summary into a numbered work item. `chat` runs your agent interactively. Then, run your first workflow:

```sh
awman exec workflow --dynamic --work-item ./path/to/spec.md
```

`awman` generates the workflow and runs it autonomously in a separate git worktree.

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
