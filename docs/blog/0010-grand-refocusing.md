# amux is becoming awman: an Agentic Workflow Manager for the entire software development lifecycle

When I started building `amux` earlier this year, the name stood for "agent multiplexer." The idea was simply "tmux, but for agents running in containers", which was immediately useful for me in my daily work. As my "agentic engineering" proficiency grew, the tool grew along with it to help standardize and automate more parts of the software development workflow.

Nine releases and 80-plus work items later, multiplexing agents across tabs is maybe 5% of what this tool does. The other 95% is everything that happens before, after, and around the agent run: writing specs, defining multi-step workflows, managing container lifecycles, running setup and teardown phases, queuing jobs on remote machines, creating pull requests, and stitching the whole thing together into a repeatable pipeline that takes you from "I have an issue" to "I have a merged PR" without leaving the terminal.

The name hasn't reflected the tool for a while now, so I'm changing it; **amux is becoming awman** — the Agentic Workflow Manager.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## What's in a name?

Even though the name itself matters little, it can shape how people think about a tool, and "agent multiplexer" was no longer really "the point". Upon discovering `amux`, one would expect tmux for AI agents but instead find a spec-driven workflow engine with a multiplexing feature. Even I had to explain "well, it's called a multiplexer, but really it's more like...".

The problem that actually needs solving (the one that keeps me moving further from the original scope) is that writing code is just one small part of agent-assisted development. There are many great agents perfectly capable of writing code, but there is a whole lot of space between "here's what I want built" and "here's a merged PR". Much of the process is still a human sitting in a terminal typing prompts, checking diffs, running tests, pushing branches, and opening PRs.

The re-focused goal of `awman` is to be the Software Development Lifecycle glue that holds agentic engineering workflows together. It's the layer that orchestrates all of the steps in and around and your agents, giving both sides predictability: the agent gets a rich spec, a safe container, and a structured workflow to follow. You get visibility, control, and the ability to define your process and reuse it predictably.

## What awman actually does now

I want to lay out the full picture, because the piecemeal release posts over the past few months haven't told the story as a whole.

**Spec-driven development.** `awman` helps manage the living documents that define agentic engineering; the structured specs that capture your architecture, security constraints, conventions, and work items. When you hand a task to an agent, it needs project- and task-level specifications if there is any hope of getting quality output. The spec is the contract between you and your agent, and it persists across sessions, agents, and team members.

**Containerized execution.** Every agent runs in a container built from your project's `Dockerfile.dev`. The agent can read and write your project's files, nothing else. No SSH keys, no credentials, no system access. When the agent finishes, the container is gone. This isn't optional, it's the main security boundary for safe agent usage. You can extend it with overlays for additionaly directories, SSH access or Docker-in-Docker when you need to, but the default is locked down.

**Multi-step workflows.** Define a pipeline in TOML or YAML: plan, implement, test, review, commit, or anything you need. Each step runs in its own container with its own prompt. awman manages the DAG, persists workflow state, and lets you pause, resume, skip, or restart steps. An `awman` workflow is the needed structure that turns a one-shot agent prompt into a repeatable, auditable process.

**Setup and teardown phases.** Workflows can now define what happens before the first agent step and after the last one. Clone a repo, create a worktree, install dependencies in setup. Run tests, commit changes, push a branch, create a pull request in teardown. All of it runs inside containers — nothing executes on your host machine. This is how you go from "submit a job" to "receive a PR" without touching the keyboard.

**Three frontends, one engine.** The CLI, TUI, and API server are thin presentation layers over a shared command engine. Commands work identically across all three. The CLI is for scripting and CI. The TUI is for interactive development. The API is for remote machines and agent clusters. Same capabilities with 3 different interaction models depending on your needs, preference, and scale.

**Remote execution and job queues.** The API server creates parallel sessions (a local directory or a remote git repo that it clones, provisions, and manages). Submit jobs to a queue and workers pick them up. Poll for status. Fetch workflow state. Run your agents on a beefy server in the closet while you work from your laptop.

**Agent-agnostic.** Claude, Codex, Gemini, Copilot, OpenCode, Maki, Crush, Cline — awman doesn't care which agent you use. The agent is just a process inside a container. Swap agents per workflow step if you want. The workflow, the spec, and the container are all managed by awman.

## The SDL pipeline

The vision I've been building toward — and the reason for the rename — is a complete software development lifecycle pipeline driven by `awman`:

1. **Spec.** Write or generate detailed specifications for work that needs to be done, either from your own ideas or from an issue tracker. This is the workflow input.
2. **Plan.** Define how you want your team of agents to accomplish the work: plan → implement → test → review. Or whatever your preferred process is. `awman` workflows are structured `.toml` files and are intended to be reusable, forkable, shareable.
3. **Setup.** `awman` provisions the environment: clones the repo, creates a branch, builds a dev container for the agent to run in, runs your workflow's setup scripts or commands.
4. **Execute.** Each workflow step runs an agent in a container against the spec with provided inputs and overlays like directories, env vars, extra skills, etc. `awman` manages state, handles failures, and lets you intervene when needed.
5. **Teardown.** `awman` runs tests, commits changes, pushes the branch, opens a pull request, or anything else you define. All running inside your project's dev container with only the credentials you allow.
6. **Review.** You review the PR, and decide if you need to run further workflows to address human feedback.

That's the general outline of the SDL, but each and every step is defined by you for your project's specific needs. Issue to merged PR is the goal, but it's up to you to decide how much is performed by agents and how much human involvement is needed. You define the workflow once, write the spec for each task, and let `awman` handle the glue that stick it all together. The "multiplexer" part — running multiple agents in parallel tabs — is still there, and it's still useful, but it's not really the point anymore.

## What to expect from awman v0.9.0

The first release under the new name will ship three major changes alongside the rename itself:

**The rename.** The tool becomes `awman`. Config moves from `~/.amux/` to `~/.awman/` and `.amux/` to `.awman/` in your repos. If you have existing config, `awman` auto-migrates it on first run — no manual steps. The "headless" mode is now called "API mode" everywhere, because that's what it is. Old `AMUX_*` environment variables will emit deprecation warnings pointing you to the new `AWMAN_*` equivalents.

**Queue-and-worker execution.** The API server moves from synchronous request-response to an async job queue backed by SQLite. Submit workflow jobs via the API, and workers pick them up to be executed. Sessions can be `local` (pointed at an existing directory) or `remote` (awman clones a git repo into an isolated directory for you). Multiple workers run concurrently — configurable via `awman.workers` in your global config.

**Workflow setup and teardown.** Workflows gain `[[setup]]` and `[[teardown]]` sections. Run setup scripts, install dependencies, or sync branches before the agent workflow as needed. Run tests, commit, push, and open PRs after the last step. All execution happens inside the project's base image defined by your Dockerfile.dev. Markdown workflow files are being removed as parsing them was unreliable — TOML and YAML only from here on out, because structured data should be structured. If you have `.md` workflows, you'll get a clear error message telling you to convert.

Together, these changes start moving the project towards a full SDL pipeline tool. You can define a workflow that encapsulates your own personal way of working with code and agents and execute it repeatably across projects and issues. You can build personal libraries of skills, workflows, and prompts to refine how you collaborate with agents to maximize your time and effort. I hope you'll give `awman` a try and let me know how it goes!

---

More at [prettysmart.dev](https://prettysmart.dev). The rename and new release will land later this week. Feedback and contributions welcome!
