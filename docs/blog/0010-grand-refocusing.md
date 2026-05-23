# amux is becoming awman: an Agentic Workflow Manager for the entire software development lifecycle

When I started building amux earlier this year, the name stood for "agent multiplexer." The idea was simply "tmux, but for agents running in containers", which was immediately useful for me in my daily work. As my "agentic engineering" proficiency grew, the tool grew along with it to help standardize and automate more parts of software development workflow.

Nine releases and 80-plus work items later, multiplexing agents across tabs is maybe 5% of what this tool does. The other 95% is everything that happens before, after, and around the agent run: writing specs, defining multi-step workflows, managing container lifecycles, running setup and teardown phases, queuing jobs on remote machines, creating pull requests, and stitching the whole thing together into a repeatable pipeline that takes you from "I have an issue" to "I have a merged PR" without leaving the terminal.

The name hasn't reflected the tool for a while now. So I'm changing it.

**amux is now awman** — the Agentic Workflow Manager.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## What's in a name?

Even though the name itself matters little, it can shape how people think about a tool, and "agent multiplexer" was no longer really "the point". Upon discovering amux, one would expect tmux for AI agents but instead find a spec-driven workflow engine with a multiplexing feature. Even I had to explain "well, it's called a multiplexer, but really it's more like...".

The problem that actually needs solving (the one that keeps me moving further from the original scope) is that writing code is just one small part of agent-assisted development. There are many great agents perfectly capable of writing code, but there is a whole lot of space between "here's what I want built" and "here's a merged PR" is still a human sitting in a terminal, typing prompts, checking diffs, running tests, pushing branches, and opening PRs. Over and over, for every task, every day.

awman is that glue. It's the layer that sits between your intent and your agents, giving both sides what they need: the agent gets a rich spec, a safe container, and a structured workflow to follow. You get visibility, control, and the ability to define your process once and reuse it.

## What awman actually does now

I want to lay out the full picture, because the piecemeal release posts over the past few months haven't told the story as a whole.

**Spec-driven development.** Every project gets an `aspec/` directory — structured documents that capture your architecture, security constraints, conventions, and work items. When you hand a task to an agent, it reads the full spec. No more spending the first ten minutes of every session re-establishing context through conversation. The spec is the contract between you and your agent, and it persists across sessions, agents, and team members.

**Containerized execution.** Every agent runs in a Docker container built from your project's `Dockerfile.dev`. The agent can read and write your project files. Nothing else. No SSH keys, no credentials, no system access. When the agent finishes, the container is gone. This isn't optional — it's the security model. You can extend it with overlays for SSH access or Docker-in-Docker when you need to, but the default is locked down.

**Multi-step workflows.** Define a pipeline in TOML or YAML: plan, implement, test, review, commit. Each step runs in its own container with its own prompt. awman manages the DAG, persists state to disk, and lets you pause, resume, skip, or restart steps. The workflow is the thing that turns a one-shot agent prompt into a repeatable, auditable process.

**Setup and teardown phases.** Workflows can now define what happens before the first agent step and after the last one. Clone a repo, create a worktree, install dependencies in setup. Run tests, commit changes, push a branch, create a pull request in teardown. All of it runs inside containers — nothing executes on your host machine. This is how you go from "submit a job" to "receive a PR" without touching the keyboard.

**Three frontends, one engine.** The CLI, TUI, and API server are thin presentation layers over a shared command engine. Every command works identically across all three. The CLI is for scripting and CI. The TUI is for interactive development. The API is for remote machines and automation. Same behavior, guaranteed by construction.

**Remote execution and job queues.** The API server accepts sessions — point it at a local directory or a remote git repo and it clones, provisions, and manages the working environment. Submit jobs to a queue and workers pick them up. Poll for status. Fetch workflow state. Run your agents on a beefy server in the closet while you work from your laptop.

**Agent-agnostic.** Claude, Codex, Gemini, Copilot, OpenCode, Maki, Crush, Cline — awman doesn't care which agent you use. The agent is just a process inside a container. Swap agents per workflow step if you want. The workflow, the spec, and the container are what awman manages. The agent is what you plug in.

## The SDL pipeline

The vision I've been building toward — and the reason for the rename — is a complete software development lifecycle pipeline driven by awman:

1. **Spec.** Write or generate a work item in `aspec/work-items/`. This is the input.
2. **Workflow.** Define the steps: plan → implement → test → review. Or whatever your preferred process is. This is reusable.
3. **Setup.** awman provisions the environment: clones the repo, creates a branch, builds the dev container.
4. **Execute.** Each workflow step runs an agent in a container against the spec. awman manages state, handles failures, and lets you intervene.
5. **Teardown.** awman runs tests, commits changes, pushes the branch, and opens a pull request. Automatically. Inside a container.
6. **Review.** You review the PR. The agent did the work. You own the decision.

That's the loop. Issue to merged PR. You define the workflow once. You write the spec for each task. awman handles everything in between. The "multiplexer" part — running multiple agents in parallel tabs — is still there, and it's still useful. But it's a feature, not the identity.

## What to expect from awman v0.9.0

The first release under the new name will ship three major changes alongside the rename itself:

**The rename.** The binary is `awman`. Config moves from `~/.amux/` to `~/.awman/` and `.amux/` to `.awman/` in your repos. If you have existing config, awman auto-migrates it on first run — no manual steps. The "headless" mode is now called "API mode" everywhere, because that's what it is. Old `AMUX_*` environment variables will emit deprecation warnings pointing you to the new `AWMAN_*` equivalents.

**Queue-and-worker execution.** The API server moves from synchronous request-response to an async job queue backed by SQLite. Submit jobs, workers pick them up, poll for status. Sessions can be `local` (pointed at an existing directory) or `remote` (awman clones a git repo into an isolated directory for you). Multiple workers run concurrently — configurable via `awman.workers` in your global config.

**Workflow setup and teardown.** Workflows gain `[[setup]]` and `[[teardown]]` sections. Clone repos, create worktrees, install dependencies before the first step. Run tests, commit, push, and open PRs after the last step. All execution happens inside containers. Markdown workflow files are dropped — TOML and YAML only, because structured data should be structured. If you have `.md` workflows, you'll get a clear error message telling you to convert.

Together, these changes complete the foundation for the full SDL pipeline. You can define a workflow that takes a git repo URL and a work item, provisions a fresh environment, runs your agents through a multi-step process, and delivers a pull request — all submitted as a single API call or CLI command.

The multiplexer isn't going anywhere. It's still a great way to work. But the name on the tin now matches what's inside.

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback and contributions welcome.
