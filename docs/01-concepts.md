# Concepts

These are the pieces you will see as you use awman. Start with [Getting started](00-getting-started.md) if you have not run a session yet.

## Isolated agent sessions

awman launches each agent in a container or microVM with your project mounted inside. The agent does not get your whole machine. You can grant specific files, environment values, skills, or SSH access with [overlays](08-overlays.md). A normal `awman ready` also makes a limited host-side agent check for authentication; see [Security and isolation](04-security-and-isolation.md).

`awman init` sets a default agent and prepares the project image. `Dockerfile.dev` describes your project's tools; `.awman/Dockerfile.<agent>` adds the agent. [Runtimes](11-runtimes.md) explains how image preparation differs outside Docker.

## Ways to work

| If you want to… | Use | Read |
|---|---|---|
| Talk to an agent | `awman` then `chat`, or `awman chat` | [TUI](02-using-the-tui.md), [agent sessions](03-agent-sessions.md) |
| Run one task and exit | `awman exec prompt "…"` | [Agent sessions](03-agent-sessions.md) |
| Run several planned steps | `awman exec workflow <file>` | [Workflows](05-workflows.md) |
| Let an agent design the steps | `awman exec workflow --dynamic --work-item <ref>` | [Dynamic workflows](06-dynamic-workflows.md) |
| Run tasks from a script or another machine | The daemon API or `awman remote` | [API and remote mode](09-api-and-remote-mode.md) |
| Delegate recurring work | `awman squad` | [Squad](12-squad.md) |

The [command reference](14-command-reference.md) lists the exact syntax and flags.

## Work items and workflows

A **work item** is a task description, usually a numbered Markdown file in `aspec/work-items/`. Project specs in `aspec/` are optional; they give agents context about your codebase. Use `awman new spec` to draft a work item.

A **workflow** is a TOML or YAML file that orders agent prompts and optional setup and teardown steps. Use `awman new workflow` to draft one, or `--dynamic` to have a leader agent design steps for a work item. Workflows can use a Git worktree so their changes stay separate until you review them. See [Workflows](05-workflows.md) and [Worktree isolation](04-security-and-isolation.md#worktree-isolation).

## Configuration

Repository settings live in `.awman/config.json`; personal settings live in `~/.awman/config.json`. Command flags and environment overrides take priority over both. See [Configuration](07-configuration.md) for examples and the full key reference.

[← Getting started](00-getting-started.md) · [Back to contents](contents.md) · [Next: Using the TUI →](02-using-the-tui.md)
