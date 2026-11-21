# awman documentation

Start with [Getting started](00-getting-started.md). Then use the guides for what you want to do; the references are for exact syntax and settings.

## Learn the basics

1. [Getting started](00-getting-started.md) — install, set up a repository, and start an agent.
2. [Concepts](01-concepts.md) — sessions, work items, workflows, and isolation.
3. [Using the TUI](02-using-the-tui.md) — navigate tabs, agent windows, and workflow controls.
4. [Agent sessions](03-agent-sessions.md) — chat, one-off tasks, permissions, authentication, and settings profiles.
5. [Security and isolation](04-security-and-isolation.md) — mounts, worktrees, credentials, and host access.

## Get work done

- [Workflows](05-workflows.md) — create and run repeatable, multi-step work.
- [Dynamic workflows](06-dynamic-workflows.md) — let an agent design the steps for a work item.
- [GitHub integration](10-github-integration.md) — use issues and pull requests.
- [Squad](12-squad.md) — run recurring tasks with a group of agents.
- [Web app](20-webapp.md) — connect a browser to the local daemon.

## Set up and operate

- [Configuration](07-configuration.md) — set defaults and inspect all config keys.
- [Overlays](08-overlays.md) — give an agent specific files, variables, skills, or context.
- [Runtimes](11-runtimes.md) — choose Docker, Apple Containers, Docker Sandboxes, or the builtin microVM.
- [Custom agents](15-custom-agents.md) — define an agent that is not built in.
- [Cleaning up](13-cleaning-up.md) — remove stopped sessions and completed workflow data.
- [The daemon](16-daemon.md) and [engine frontends](19-engine-and-frontends.md) — run shared background services and daemon-backed TUI sessions.
- [Workers and work servers](17-workers-and-work-servers.md) — connect to work queues and run pulled jobs.
- [API and remote mode](09-api-and-remote-mode.md) — automate with HTTP or `awman remote`.

## Reference

- [Command reference](14-command-reference.md) — all commands and flags, generated from the command catalogue.
- [Configuration keys](07-configuration.md#reference-all-keys) — exact settings and defaults.
- [HTTP API](09-api-and-remote-mode.md#http-api) and [Work Server Protocol](17-workers-and-work-servers.md#work-server-protocol-v1-reference) — integration contracts.

Contributing to awman? See the [architecture overview](architecture.md) and the authoritative [specs](../aspec/).
