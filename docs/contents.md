# awman Documentation

A guide to using awman, the containerized multi-agent terminal multiplexer.

---

## Contents

| # | File | What's covered |
|---|------|----------------|
| 00 | [Getting Started](00-getting-started.md) | Installation, first agent session |
| 01 | [Concepts](01-concepts.md) | Mental model: containers, agents, modes, overlays |
| 02 | [Using the TUI](02-using-the-tui.md) | TUI layout, tabs, container window, keyboard reference |
| 03 | [Agent Sessions](03-agent-sessions.md) | `chat`, work items, agent authentication |
| 04 | [Security & Isolation](04-security-and-isolation.md) | Worktrees, overlays, Docker socket, container transparency |
| 05 | [Workflows](05-workflows.md) | Multi-step workflows, control board, state persistence |
| 06 | [Yolo Mode](06-yolo-mode.md) | Fully autonomous operation, disallowed tools, countdown |
| 07 | [Headless Mode](07-headless-mode.md) | TTY detection, non-interactive operation, CI/CD integration |
| 08 | [Configuration](08-configuration.md) | Config files, runtime selection, all fields |
| 09 | [Overlays](09-overlays.md) | `dir()`, `env()`, `skill()`, `ssh()`, `context()` — sources and merge semantics |
| 10 | [API Mode](10-api-mode.md) | HTTP server, sessions, commands, CI/automation |
| 11 | [Remote Mode](11-remote-mode.md) | `remote exec`, `remote session`, live log streaming, TUI pickers |
| 12 | [Architecture Overview](12-architecture-overview.md) | Layer design, design principles |
| 13 | [GitHub Integration](13-github-integration.md) | `--issue` flag, fetching issues, authentication |
| 14 | [Context Overlays](14-context-overlays.md) | Persistent context, system prompts, global/repo/workflow scopes |
| 15 | [Mouse & TUI Agents](15-mouse-and-tui-agents.md) | Scroll forwarding, text selection, agent mouse tracking |
| — | [Architecture (Detailed)](architecture.md) | Source layout, in-depth design decisions |

---

Start with [Getting Started](00-getting-started.md) if this is your first time.
