# Getting Started with amux

`amux` is a terminal multiplexer for AI code agents. Every agent action runs inside a Docker container — never directly on your machine. This makes agent sessions reproducible, isolated, and safe to run autonomously.

This guide walks you through the core concepts and gets you to your first working agent session.

---

## Core concepts

Before running a single command, it helps to understand what amux is actually doing.

### Why containers?

When a code agent runs on your machine directly, it has access to your home directory, SSH keys, credentials, environment variables, and everything else your user account can touch. A bug in the agent — or a poorly-scoped task — can cause unintended side effects.

amux solves this by running agents inside containers. The container only sees your project directory (mounted read-only by default). Your credentials are injected as environment variables, not mounted as files. Your SSH keys are never exposed unless you explicitly opt in. The container is thrown away when the session ends.

### Container image setup

amux uses a two-layer image system that separates your project's build environment from the AI agent tooling.

**`Dockerfile.dev`** (at the Git root) is the _project base image_. It defines the OS, language runtimes, build tools, and test dependencies specific to your project — nothing agent-specific. It produces the image `amux-{project}:latest`.

**`.amux/Dockerfile.{agent}`** (in the `.amux/` directory) is the _agent image_. It extends the project base (`FROM amux-{project}:latest`) and installs the AI agent tooling for whichever agent you are using (Claude Code, Codex, OpenCode, Maki, Gemini, GitHub Copilot CLI, Crush, or Cline). It produces `amux-{project}-{agent}:latest` — the image that actually runs your agent sessions.

amux ships templates for both files. The **agent audit** (run via `amux ready --refresh` or during `amux init`) launches an agent to inspect your codebase and updates `Dockerfile.dev` with the exact tools your project needs. Agent dockerfiles are generated from templates maintained by amux and rarely need manual edits.

Keeping these two files separate means you can update project tooling without touching the agent setup, and switch between or update agents without rebuilding your entire project environment.

Both files should be committed to source control — teammates get the same image setup when they clone the repo.

### What is `aspec`?

`aspec` is an optional folder of Markdown specification files that describe your project to agents. Before writing any code, agents read these specs to understand the architecture, security constraints, coding conventions, and what "done" looks like for a given task.

The spec folder typically contains:

| File | Purpose |
|------|---------|
| `foundation.md` | Project purpose, language choices, personas |
| `architecture/design.md` | Patterns, module layout, design decisions |
| `architecture/security.md` | Security constraints (non-negotiable) |
| `uxui/cli.md` | CLI commands, flags, and config |
| `devops/localdev.md` | How to build, test, and run locally |
| `work-items/` | Individual feature, bug, task, and enhancement specs |

`aspec` is optional but strongly recommended. Without it, agents have to guess at the intent and context behind a task. With it, agents make decisions consistent with the rest of the codebase.

### Work items

A work item is a Markdown file that describes a specific piece of work: a feature, bug fix, enhancement, or task. Work items follow a numbered naming convention (`0001-add-auth.md`, `0002-fix-crash.md`) and contain everything the agent needs to implement, test, and document the change.

By default, amux looks for work items in `aspec/work-items/`. If your repo uses a different folder structure, you can configure the path:

```sh
amux config set work_items.dir docs/work-items
```

Work items can be executed via workflows using `amux exec workflow`. See the [Workflows](04-workflows.md) and [Agent Sessions](02-agent-sessions.md) guides for more details.

---

## Prerequisites

- **Git** — your project must be in a Git repository
- **A container runtime** — one of:
  - **Docker** (default, all platforms) — a running Docker daemon ([install Docker](https://docs.docker.com/get-docker/))
  - **Apple Containers** (macOS 26+ only) — Apple's native `container` CLI; no Docker Desktop required
- **A supported agent** — one of: Claude Code, OpenAI Codex, OpenCode, Maki, Google Gemini CLI, GitHub Copilot CLI, Crush, or Cline installed and authenticated on your machine

---

## Installation

```sh
curl -s https://prettysmart.dev/install/amux.sh | sh
```

The installer detects your platform and installs `amux` to `/usr/local/bin`.

<details>
<summary>Other installation options</summary>

**With mise** — using the [GitHub backend](https://mise.jdx.dev/dev-tools/backends/github.html):

```sh
mise use -g github:prettysmartdev/amux
```

To pin to a specific version: `mise use -g github:prettysmartdev/amux@0.7.0`

**From GitHub Releases** — download the binary for your platform from the [Releases page](https://github.com/prettysmartdev/amux/releases), make it executable, and move it onto your `PATH`:

| Platform | Asset |
|----------|-------|
| Linux (x86_64) | `amux-linux-amd64` |
| Linux (ARM64) | `amux-linux-arm64` |
| macOS (Intel) | `amux-macos-amd64` |
| macOS (Apple Silicon) | `amux-macos-arm64` |
| Windows (x86_64) | `amux-windows-amd64.exe` |

```sh
chmod +x amux-*
mv amux-* /usr/local/bin/amux
```

**From source** — requires Rust 1.94+ and `make`:

```sh
git clone https://github.com/prettysmartdev/amux.git
cd amux
make install    # builds and installs to /usr/local/bin/amux
```

</details>

---

## First-time project setup

Navigate to your project's Git root and run:

```sh
amux init
```

This sets up your project for use with amux. The process is interactive and walks you through each phase:

### Phase 1: Agent selection

```
Which agent would you like to use? [claude/codex/opencode/maki/gemini]:
```

Choose your preferred code agent. This becomes the default for all future `amux` commands in this repo. You can change it later with `amux config set agent codex` or pick a different agent for a specific command with the `--agent` flag.

### Phase 2: Dockerfile setup

amux creates two Dockerfile templates:

- **`Dockerfile.dev`** at the Git root (your project's build environment)
- **`.amux/Dockerfile.{agent}`** in the `.amux/` directory (agent-specific tooling)

Both are created from templates and should be committed to source control so teammates get identical environments.

### Phase 3: Agent audit (optional)

```
Run the agent audit? This inspects your project and updates Dockerfile.dev with the tools you actually need. [y/N]:
```

The audit launches a container that reads your codebase, detects what you're building (Python? Node? Rust? Docker?), and updates `Dockerfile.dev` with the exact build tools, language runtimes, and test dependencies your project needs. **It's strongly recommended** — it's the main value of having `Dockerfile.dev` as a separate file.

If you decline, `Dockerfile.dev` will be a minimal debian base, and you'll need to manually add any tools your agents need later.

### Phase 4: Work items setup (if needed)

If you didn't pass `--aspec` and no `aspec/` folder already exists, amux offers to set up work item handling:

```
Set up work items? You can configure a custom work items directory or use the bundled aspec/ template. [y/N]:
```

**Accept:** amux walks you through two options:

1. **Download the bundled `aspec/` template** — gives you spec templates and work item scaffolding matching the `aspec/` standard layout.
2. **Configure a custom directory** — point amux to an existing `docs/specs/`, `workitems/`, or other directory where you keep work item files.

Either way, amux writes the configuration so that `new spec` and workflow commands can find your work item files without extra flags.

**Decline:** You can set this up later with `amux init --aspec` or `amux config set work_items.dir docs/specs`.

### Phase 5: Image building

amux builds the project base image and agent image. This may take a few minutes on first run, depending on your `Dockerfile.dev` and network speed.

### Init summary

When complete, amux shows a summary table:

```
┌──────────────────────────────────────────────────┐
│              Init Summary (claude)                │
├───────────────────┬──────────────────────────────┤
│            Config │ ✓ saved                       │
│      aspec folder │ ✓ downloaded (--aspec flag)   │
│    Dockerfile.dev │ ✓ created                     │
│  Agent dockerfile │ ✓ created                     │
│       Agent audit │ ✓ completed                   │
│      Base image   │ ✓ built                       │
│      Agent image  │ ✓ built                       │
│       Work items  │ ✓ configured                  │
└───────────────────┴──────────────────────────────┘
```

Each row indicates whether that phase was skipped (`–`), succeeded (`✓`), or encountered an error (`✗`).

### Download aspec separately

To also download the `aspec/` folder with spec templates and work item scaffolding:

```sh
amux init --aspec
```

This flag is only needed if you want the full `aspec/` folder — the work items setup prompt (Phase 4 above) already offers to download it interactively.

---

## Verifying your environment

After init, run:

```sh
amux ready
```

This checks:

1. That your container runtime (Docker or Apple Containers) is available and running
2. That `Dockerfile.dev` and `.amux/Dockerfile.{agent}` exist and are configured
3. That your agent (e.g. Claude Code) is installed and authenticated — it sends a test greeting and shows the response
4. That both the project base image and the agent image have been built

If everything is green, you're ready to run agents.

### Re-running the Dockerfile audit

If your project's toolchain has changed (you added a new language, test framework, or dependency), update your `Dockerfile.dev` by re-running the audit:

```sh
amux ready --refresh
```

This launches the audit agent, updates `Dockerfile.dev` (the project base), and rebuilds both images. The agent dockerfile is not modified — it is managed by amux templates and contains only agent tooling. You should commit the updated `Dockerfile.dev` to source control.

---

## Opening the TUI

```sh
amux
```

This opens the interactive TUI. You'll see:

- A **tab bar** at the top (one tab per project session)
- An **execution window** in the middle (shows command output)
- A **command input box** at the bottom

Type any amux subcommand (like `chat`) and press **Enter** to run it. The TUI supports autocomplete — start typing and suggestions appear below the input.

---

## Your first agent session

### Freeform chat

```sh
chat
```

(Type this in the TUI command box and press Enter, or run `amux chat` from your terminal.)

This launches an agent session in a container against your project. A **container window** opens over the execution window — this is a full terminal emulator connected to the agent. You can type directly to the agent, ask questions, request changes, and see output in real time.

Press **Ctrl-M** to toggle the container window between maximized and minimized (the agent keeps running in the background either way). When the window is maximized, **Esc** and other terminal keys are forwarded directly to the agent.

### Implementing a work item

To execute a work item, create a workflow that references it, then run the workflow with `amux exec workflow`. See the [Workflows](04-workflows.md) guide for how to create and run workflows.

For example, to run a workflow bundled with amux:

```sh
amux exec workflow aspec/workflows/implement-feature.md --work-item 0001
```

The agent reads the work item spec, writes code, runs tests, and reports back — all inside a container.

---

## Creating work items

```sh
new spec               # prompts for type and title, creates the file
new spec --interview   # creates the skeleton, then opens an agent to help fill it out
```

Four work item types are available: Feature, Bug, Task, and Enhancement.

Work items are created in the configured work items directory (defaulting to `aspec/work-items/`). If you haven't run `amux init --aspec` and haven't configured `work_items.dir`, amux will prompt you to auto-discover a template or create the file with a minimal stub. You can configure a custom directory at any time:

```sh
amux config set work_items.dir docs/work-items
```

With `--interview`, after you provide a brief summary, the agent asks clarifying questions and writes out the full spec (user stories, implementation plan, edge cases, test plan) before any implementation starts.

After implementing a work item, you can have the agent update the spec to match what was actually built:

```sh
specs amend 0001
```

## Creating workflows and skills

The `new` subcommand is a unified entry point for creating amux artefacts:

```sh
new spec                # prompts for type and title, creates a work item file
new workflow            # interactively build a workflow file step by step
new workflow --interview  # let an agent write the workflow from a summary
new skill               # interactively create a Claude Code skill file
new skill --interview   # let an agent write the skill body from a summary
```

Both `new workflow` and `new skill` accept `--global` to write to `~/.amux/` instead of the current repo, building a personal library that travels across projects. See [Workflows](04-workflows.md#creating-a-workflow-file) and [Creating skills](02-agent-sessions.md#creating-skills) for full details.

---

## What's next

- **[Using the TUI](01-using-the-tui.md)** — tabs, keyboard shortcuts, container window controls, scrollback
- **[Agent Sessions](02-agent-sessions.md)** — all `chat` flags, authentication, work item management
- **[Security & Isolation](03-security-and-isolation.md)** — worktrees, SSH keys, Docker socket access
- **[Workflows](04-workflows.md)** — multi-step agent runs with plan, review, and documentation phases
- **[Yolo Mode](05-yolo-mode.md)** — fully autonomous operation for long-running tasks
- **[Configuration](07-configuration.md)** — all config file options

---

[Next: Using the TUI →](01-using-the-tui.md)
