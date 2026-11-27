# Getting started

awman runs coding agents in isolated containers or microVMs. This guide uses Docker, the default runtime. For other runtimes, see [Runtimes](11-runtimes.md).

## Before you start

You need a Git repository, a running Docker daemon, and an account with a [supported agent](03-agent-sessions.md). Run these commands from the repository root.

## Install

```sh
curl -s https://prettysmart.dev/install/awman.sh | sh
```

The installer puts `awman` in `/usr/local/bin`. On Apple Silicon macOS or Linux, you can instead use Homebrew:

```sh
brew install prettysmartdev/tap/awman
brew upgrade awman
```

You can also [download a release](https://github.com/prettysmartdev/awman/releases), run `mise use -g github:prettysmartdev/awman`, or build from source with `make install`. Intel Macs are unsupported; the Linux x86_64 release needs glibc 2.35 or newer. See [Runtimes](11-runtimes.md) for platform details.

## Set up the repository

```sh
awman init                 # Claude Code
awman init --agent codex   # choose Codex instead
```

Follow the prompts to select or create `Dockerfile.dev` and, if useful, let the agent adapt it to your project's toolchain. Work item setup is optional. Commit `Dockerfile.dev` and `.awman/` to share the environment with your team.

## Check readiness

```sh
awman ready
```

This checks the runtime, images, and agent authentication. If you need images before logging in, use `awman ready --prepare-only`; it reports authentication as **not checked**. See [Agent sessions](03-agent-sessions.md#preparing-images-without-authentication-ready---prepare-only).

## Start an agent

```sh
awman
```

In the TUI command box, type `chat` and press **Enter**. Talk to the agent in the terminal window. **Ctrl-M** cycles the window through maximized, minimized, and hidden without stopping the session. To start directly from your shell, run `awman chat`.

## Give awman a task

Once your agent is ready, you can give it a work item and let it plan and run a workflow in a separate Git worktree:

```sh
awman new spec
awman exec workflow --dynamic --work-item 0042
```

`new spec` asks for a title and summary, then an agent writes the work item. Use the number it creates in the workflow command. You can also use a GitHub issue with `--work-item github:84` or a Markdown file path. Before starting, awman checks the workflow's image, Git and credential prerequisites. See [Dynamic workflows](06-dynamic-workflows.md) for options and [Workflows](05-workflows.md) for writing your own steps.

## Use a work server

To take tasks offered by a work server, use `awman link <server-url>`, then `awman queue` or the TUI's **Ctrl+Q** queue panel. The worker uses the same command engine and agent isolation as your own sessions. See [Workers and work servers](17-workers-and-work-servers.md) for setup and automatic claiming.

## Where to go next

- [Concepts](01-concepts.md) explains the main pieces.
- [Using the TUI](02-using-the-tui.md) covers navigation and shortcuts.
- [Agent sessions](03-agent-sessions.md) covers agents, permissions, authentication, and one-off prompts.
- [Workflows](05-workflows.md) covers multi-step work; [dynamic workflows](06-dynamic-workflows.md) let an agent design the steps.
- [Command reference](14-command-reference.md) lists every command and flag.

[Back to contents](contents.md) · [Next: Concepts →](01-concepts.md)
