# Context Overlays

Context overlays give you and your agents a persistent, shared workspace on disk — combined with automatic system prompt instructions explaining what that workspace is and how to use it. They solve a critical problem: how do you avoid re-explaining the same project context, developer preferences, or workflow state to every agent you run?

---

## What are context overlays?

A context overlay is:

1. **A directory on your host** — persistent files you create and manage, located under `~/.awman/context/`
2. **A system prompt** — automatically injected into the agent's instructions, explaining the context and what files to expect
3. **Unified into one syntax** — expressed as `context(scope[:permission])` just like other overlays

Three scopes are supported:

| Scope | Location | Purpose | Shared across |
|-------|----------|---------|----------------|
| **global** | `~/.awman/context/global/` | Your personal coding preferences, style rules, recurring mistakes to avoid | All projects and workflows |
| **repo** | `~/.awman/context/repo/{owner}/{repo}/` | Project-specific architecture notes, gotchas, accumulated team knowledge | All agents working on this repo |
| **workflow** | `~/.awman/context/workflow/` (per workflow invocation) | Shared state and coordination messages between steps in a multi-agent workflow | All steps in the same workflow run |

---

## When to use context overlays

### Global context: across all projects

Use `context(global)` when you have **standing guidance** that applies everywhere:

- Personal coding style preferences (naming conventions, code structure, patterns you prefer)
- Common gotchas and recurring mistakes you've learned to avoid
- Standing architectural decisions or patterns you always want followed
- Links to frequently-referenced documentation or team wikis
- Scripts or templates you reuse across projects

**Example:** You maintain a `~/.awman/context/global/coding-style.md` that says "Always use async/await, never callbacks. Prefer composition over inheritance." Every agent you run, regardless of project, reads that guidance and applies it.

### Repo context: project-specific knowledge

Use `context(repo)` when you have **project-specific** knowledge worth preserving:

- Architecture decisions and design patterns unique to this codebase
- Known workarounds or technical debt you want agents to be aware of
- Team conventions that differ from your personal style
- Links to key documentation, ADRs, or architectural decision records
- Notes from previous agent sessions ("we tried X, it didn't work because Y")

**Example:** After an agent successfully refactors your authentication layer, you add notes to `~/.awman/context/repo/myorg/myproject/auth-layer.md`: "The auth code is in `src/auth.rs`. Mutations must go through the `SessionManager`. Earlier attempts to refactor failed because of the state machine in `refresh_token()` — refer to git commit abc123 for context."

The next agent working on auth code reads this and avoids pitfalls the previous agent discovered.

### Workflow context: multi-step coordination

Use `context(workflow)` when running **multi-step workflows** where each step needs to see:

- Which steps have completed and which are pending
- Notes and intermediate artifacts left by earlier steps
- Shared workspace for coordination (e.g. step 1 creates a design doc, step 2 reads it and implements)

**Example:** Your workflow has three steps: `plan`, `implement`, `document`. The `plan` step writes a detailed plan to `/awman/context/workflow/plan.md`. The `implement` step reads it and builds according to it, writing progress notes. The `document` step reads both and generates docs. No step needs to re-discover what earlier steps decided.

---

## Three-scope system prompt injection

When you use `context()`, awman automatically injects a section into your agent's system prompt explaining the context and what to do with it. Here's what the agent sees (the exact wording may vary slightly):

### Global context system prompt

```
## Global Developer Context

You have access to a persistent global context directory mounted at /awman/context/global.

This directory is the developer's personal, cross-project workspace — maintained by them
and shared across all agents, projects, and workflows they run with awman. It is meant
to be portable and durable: a personalized addition to per-project CLAUDE.md files that
travels with this developer everywhere.

The directory may contain:
- Personal coding style preferences and conventions the developer always wants followed
- Notes on recurring mistakes, things to avoid, or common gotchas encountered across
  projects
- Shared tools, scripts, or templates that may be used freely
- Any standing guidance the developer wants applied to all of their work

Instructions:
1. Read all files in /awman/context/global at the start of your task to understand the
   developer's preferences and any standing guidance they have left.
2. You SHOULD write to files inside /awman/context/global to record any significant
   insights or mistakes, or leave guidance you want future agent sessions to know based on
   your interactions with the developer. The developer will review and curate these files
   over time.
3. Treat the contents of this directory as extremely valuable developer guidance and
   context — refer to it throughout your work.
```

### Repo context system prompt

```
## Repository-Specific Context

You have access to a repository-specific context directory mounted at /awman/context/repo.

This directory contains knowledge and guidance specific to the project you are currently
working in. It is maintained collaboratively by the developer and agents who have worked
on this codebase before.

The directory may contain:
- Architecture notes and key design decisions
- Project-specific conventions, patterns, and best practices
- Known gotchas, workarounds, and areas of technical debt
- Domain knowledge and business logic documentation
- Notes from previous agent sessions working on this codebase

Instructions:
1. Read all files in /awman/context/repo at the start of your task to orient yourself to
   the project-specific context.
2. You SHOULD write to files in /awman/context/repo to capture any significant insights
   discovered during your work, document decisions made, or leave guidance for future
   agent sessions. The developer will review and curate these files.
3. Treat the contents of this directory as extremely valuable project context — refer to
   it alongside the codebase itself throughout your work.
```

### Workflow context system prompt (dynamic)

In a multi-step workflow, the system prompt is regenerated for each step with live progress information:

```
## Workflow Context

You are running as part of the multi-agent "Build API Implementation" workflow,
managed by awman.

Your current step: implement (step 2 of 3)

Workflow progress:
  [✓] plan — completed
  [→] implement — in progress (this is you)
  [○] test — pending

You have access to a shared workflow context directory mounted at /awman/context/workflow.
Every agent step in this workflow shares this directory and can read and write files
there.

Instructions:
1. At the start of your task, read any files left by previous steps in
   /awman/context/workflow — they may contain intermediate results, shared state,
   helpful scripts, or instructions from earlier steps.
2. Write your outputs, notes, intermediate artifacts, scripts, investigation results,
   and any state that later steps will need into /awman/context/workflow. Use
   descriptive file names so downstream steps understand what you produced.
3. You are one step in a coordinated multi-agent workflow. Produce your deliverables
   reliably (no more, no less), document what you produced in the provided directory,
   and leave the workspace ready for the next step.
```

---

## Setting up context directories

### Global context: personal preferences

Create `~/.awman/context/global/` on your host:

```sh
mkdir -p ~/.awman/context/global
```

Add files describing your standing guidance. Examples:

**`coding-style.md`:**
```markdown
# Coding Style

## Rust
- Use async/await, never callbacks
- Prefer `Result` over panics
- Write exhaustive match statements
- Document public API items with doc comments

## Python
- Follow PEP 8
- Use type hints on all function signatures
- Prefer list comprehensions over loops when readable
```

**`common-mistakes.md`:**
```markdown
# Common Mistakes to Avoid

- Forgetting to check for None/empty before indexing (use `.get()` or pattern matching)
- Spawning long-running tasks without a shutdown hook
- Over-abstracting too early (wait until you have 3+ similar pieces before extracting)
- Assuming SQL queries are fast without profiling first
```

**`decision-log.md`:**
```markdown
# Standing Decisions

- We use Postgres, not other databases. No SQLite in user-facing code.
- Cloud deployment is via Kubernetes only. No ECS or bespoke orchestration.
- API responses use hypermedia (HATEOAS); clients should follow links, not assume URLs.
```

Then configure it in your global config:

**`~/.awman/config.json`:**
```json
{
  "overlays": ["context(global)"]
}
```

Now every agent you run gets your guidance automatically.

### Repo context: project knowledge

After working with an agent on a project, add notes to the repo context directory:

```sh
mkdir -p ~/.awman/context/repo/myorg/myproject
```

Add files specific to this project:

**`~/.awman/context/repo/myorg/myproject/architecture.md`:**
```markdown
# Architecture

## Layered design

- Layer 0: Data types, config, persistence (`src/data/`)
- Layer 1: Runtime primitives, container orchestration (`src/engine/`)
- Layer 2: Business logic, commands (`src/command/`)
- Layer 3: Presentation (CLI, TUI, API) (`src/frontend/`)

Layers only depend on lower layers. Never circular.
```

**`~/.awman/context/repo/myorg/myproject/gotchas.md`:**
```markdown
# Known Gotchas

## Workflow invocation state
- Workflow state is stored in `~/.awman/workflows/{uuid}.json`
- Changing a step's name breaks resumption (the UUID key changes internally)
- Always back up the workflow state file before major refactors

## Docker build caching
- Our Dockerfile.dev uses `COPY . .` which invalidates the build cache on every file change
- If building is slow, consider multi-stage builds or separate layer caching
- See git commit 1a2b3c for a benchmarking script
```

Configure it in your repo config:

**`.awman/config.json` (repo-local):**
```json
{
  "overlays": ["context(repo)"]
}
```

Now agents working on this repo are aware of the project's context and constraints.

### Workflow context: multi-step coordination

Workflow context directories are created automatically by awman — you don't set them up manually. Instead, you reference them in your workflow file:

**`aspec/workflows/implement.toml`:**
```toml
[workflow]
title = "Implement Feature"

overlays = ["context(global)", "context(repo)", "context(workflow)"]

[[step]]
name = "plan"
prompt = """
Review the requirements and the repo context at /awman/context/repo.
Write a detailed implementation plan to /awman/context/workflow/plan.md.
Include:
- High-level architecture
- Key files to modify
- Estimated LOC changes
- Any risks or blockers
"""

[[step]]
name = "implement"
prompt = """
Read the plan from /awman/context/workflow/plan.md.
Implement the feature according to the plan.
Write implementation notes to /awman/context/workflow/implementation-notes.md
as you go (so the next step can see what you did).
"""

[[step]]
name = "document"
prompt = """
Read the plan and implementation notes from /awman/context/workflow/.
Write user-facing documentation based on what was actually implemented.
"""
```

Each step has access to `/awman/context/workflow/` and can read/write files there. Later steps see what earlier steps produced.

---

## Agent system prompt support

Most awman-compatible agents support system prompt injection natively, so the context system prompt is automatically delivered. If your agent doesn't support it, awman still mounts the context directory, but you'll need to reference it manually in your prompt.

### Agents with native system prompt injection

These agents receive the context system prompt automatically:

| Agent | Delivery method |
|-------|-----------------|
| **claude** | CLI flag `--append-system-prompt-file` (recommended) |
| **codex** | CLI flag `--config developer_instructions=<text>` |
| **opencode** | Auto-reads `AGENTS.md` file in mounted context directory |
| **copilot** | Env var `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` |
| **antigravity (agy)** | CLI flag `--add-dir` + auto-reads `AGENTS.md` |

### Agents without native system prompt injection

These agents still have the context directory mounted, but won't receive the system prompt automatically:

| Agent | Workaround |
|-------|-----------|
| **maki** | Reference the context directory in your prompt manually (e.g. "My preferences are in /awman/context/global") |
| **crush** | Reference the context directory in your prompt manually |

### Agents with degraded system prompt support

These agents replace the entire system prompt rather than appending to it. awman prepends a baseline preamble to restore tool guidance, but the delivery is less clean:

| Agent | Note |
|-------|------|
| **gemini** (deprecated) | Env var replaces default prompt (destructive). Use `cline` or `claude` instead. |
| **cline** | CLI flag replaces default prompt (destructive). Use `claude` if possible. |

For these agents, you'll see a `warning` logged at startup indicating degraded delivery. If you're choosing a default agent, prefer `claude` for the smoothest context experience.

---

## Permission modes: read-only vs read-write

By default, context overlays are **read-write (`rw`)** — agents can modify files in the context directory, and those changes persist to the host.

### Why `rw` by default?

The entire point of context overlays is to **accumulate knowledge** across agent sessions. An agent should be able to:

- Update `~/.awman/context/global/common-mistakes.md` when it discovers a new gotcha
- Add notes to `~/.awman/context/repo/{owner}/{repo}/` after successfully completing a complex task
- Leave files in `/awman/context/workflow/` for the next step to consume

Making them read-only by default would defeat that purpose.

### When to use `:ro` (read-only)

Use read-only context when:

- **CI/CD pipelines**: You don't want agents writing to shared context in a CI environment.
  ```
  context(repo:ro), context(global:ro)
  ```

- **Shared/untrusted environments**: You want to share guidance without letting agents modify it.
  ```
  context(global:ro)  # Share my preferences, but don't let the agent add to them
  ```

- **Archival/reference**: You have a context directory that's meant to be a reference that shouldn't change.
  ```
  context(repo:ro)  # Read the project's archived knowledge, don't modify it
  ```

**Example: read-only repo context in CI:**

```toml
[workflow]
title = "CI Tests"
overlays = ["context(repo:ro)"]  # Access project knowledge, but don't modify it

[[step]]
name = "run_tests"
prompt = "Run the test suite..."
```

---

## Worked examples

### Example 1: Personal style guide across projects

You want every agent to follow your personal coding standards without repeating them in every project.

**Setup:**

```sh
mkdir -p ~/.awman/context/global
cat > ~/.awman/context/global/style.md << 'EOF'
# Style Guide

- Use descriptive variable names (cost_usd, not c)
- Avoid abbreviations (customer, not cust)
- Comments explain why, not what
- Max 80 columns for readability on small screens
- Prefer explicit over implicit (no magic numbers)
EOF

cat > ~/.awman/context/global/testing.md << 'EOF'
# Testing Standards

- Every public function has a unit test
- Integration tests for cross-component flows
- Test the happy path and 2-3 edge cases minimum
EOF
```

**Config:**

```json
{
  "overlays": ["context(global)"]
}
```

**Result:** Every agent you run reads your style guide and testing standards automatically, without you needing to repeat them in `CLAUDE.md` files.

---

### Example 2: Multi-step workflow with coordination

You're running a workflow where Step 1 makes architectural decisions that Step 2 must follow.

**Workflow file:**

```toml
[workflow]
title = "Architecture Review → Implementation"
overlays = ["context(global)", "context(repo)", "context(workflow)"]

[[step]]
name = "review"
agent = "claude"
prompt = """
Review the codebase architecture (you have repo context at /awman/context/repo).
Based on our coding style (in /awman/context/global), propose 3 refactorings
ranked by impact.

Write your proposal to /awman/context/workflow/refactoring-proposal.md.
Include:
- Refactoring #1: [description]
  Impact: X
  Effort: Y
  Risks: Z
...
"""

[[step]]
name = "implement"
agent = "claude"
depends_on = ["review"]
prompt = """
Read the refactoring proposal at /awman/context/workflow/refactoring-proposal.md.

Implement the top-ranked refactoring.

As you work, write notes to /awman/context/workflow/implementation.md:
- What you changed and why
- Any blocking issues you hit
- Design decisions you made and your reasoning

The next step will read your notes.
"""

[[step]]
name = "test"
agent = "claude"
depends_on = ["implement"]
prompt = """
Read what the implementation step did at /awman/context/workflow/implementation.md.

Write tests for the changes (especially focus on the risks noted).
Report test results to /awman/context/workflow/test-results.md.
"""
```

**Result:** Each step sees what earlier steps did and why, and leaves notes for downstream steps. The final agent can see the full decision trail.

---

### Example 3: Team project knowledge accumulation

Your team maintains project-specific context that improves over time.

**Initial state:**

```sh
mkdir -p ~/.awman/context/repo/myorg/myproject
echo "# Project context (to be populated)" > ~/.awman/context/repo/myorg/myproject/README.md
```

**After Agent Session 1** (refactoring auth):
You add notes:

```markdown
# Architecture

## Authentication Layer
- Location: src/auth.rs
- Design: SessionManager wraps refresh logic
- IMPORTANT: refresh_token() has complex state machine (see commit abc123)
- Recent refactoring attempt (earlier session): tried extracting refresh to separate module,
  failed because of state coupling. Would need to refactor state machine first.
```

**After Agent Session 2** (adding OAuth):
Agent reads your notes, avoids the refactoring trap, and adds its own:

```markdown
## OAuth Integration
- Added to src/oauth.rs
- Uses redirect flow (not client credentials)
- Stores tokens in SessionManager (see auth layer notes above)
- Future work: consider token caching to reduce API calls
```

**Next session:** The next agent has accumulated knowledge from multiple sessions without anyone having to re-explain things.

---

## Managing context effectively

### Good context practices

- **Keep files short and focused:** One `.md` file per topic (architecture, gotchas, decision log).
- **Write for future-you:** Assume the reader (another agent or your future self) has no context.
- **Link to code:** Reference git commits, file paths, and line numbers when relevant.
- **Explain why, not what:** "We use Postgres because it has the best JSON support for our use case" beats "We use Postgres."
- **Maintain it like code:** Review and curate context files periodically; remove outdated guidance.
- **Version control it if needed:** For teams, consider checking context files into the repo (e.g. `docs/context-global.md`, `docs/context-repo.md`) as a source of truth.

### Bad context practices

- **Context bloat:** Dumping every thought into context files makes them noise rather than signal.
- **Outdated guidance:** If your context says "we tried X, it failed" but you've since fixed X, update it.
- **Secrets in context:** Never put API keys, passwords, or sensitive data in context files.
- **Too vague:** "Be careful with the auth layer" is less useful than "refresh_token() mutates state; changes there block on the state lock."

---

## Troubleshooting

### Context directory not created

If you see an error like "Permission denied" when awman tries to create `~/.awman/context/global/`:

```sh
# Ensure ~/.awman exists and is writable
ls -la ~/.awman
chmod 755 ~/.awman

# Manually create the context dirs
mkdir -p ~/.awman/context/global
mkdir -p ~/.awman/context/repo
mkdir -p ~/.awman/context/workflows
```

### Context files not visible in container

Check that the context overlay is configured and the directory is mounted:

```sh
# See the full Docker command (it prints before running)
# Look for "-v /home/you/.awman/context/..." in the output

# Manually verify the host directory exists
ls ~/.awman/context/global
```

If the mount isn't there, check:
- Is `context(global)` (or `context(repo)`, `context(workflow)`) in your config or flags?
- Did awman report any warnings about the overlay?

### Agent not following context guidance

If an agent isn't following the instructions in your context:

- **Check that system prompt injection is working:** Most agents show the system prompt or you can inspect their behavior to see if they're reading the guidance.
- **For agents without native support (maki, crush):** You'll need to reference the context manually in your prompt.
- **Make sure your guidance is clear:** If it's vague, agents might ignore it. Be specific.

### Context writes not persisting

If you see an agent write to `/awman/context/global` but the file doesn't appear on the host:

- **Check permissions:** Is `~/.awman/context/global/` writable by the container user?
- **Check read-only mode:** Did you accidentally use `context(global:ro)`?
- **Check the path:** The agent must write to `/awman/context/global/` (the mounted path), not `~/.awman/context/global/` (which doesn't exist in the container).

---

## FAQs

**Q: Can I use context overlays with the TUI?**

A: Yes. Context overlays work the same way in TUI, CLI, and API modes.

**Q: What happens if two workflow invocations run simultaneously?**

A: Each gets its own `/awman/context/workflow/` directory, keyed by the workflow invocation UUID. They don't interfere.

**Q: Can I share context overlays across teams?**

A: Not directly — context directories are in `~/.awman/` on your host. For team context, consider:
- Adding team context to your repo's `docs/` and have agents read from there
- Checking context files into the repo (e.g. `.awman/context-global.md`)
- Using shared mounted directories (e.g. team Slack or wiki links in global context)

**Q: How big can context files be?**

A: There's no hard limit, but keep them readable. If a context file exceeds a few thousand lines, consider splitting it.

**Q: Can context overlays contain binary files?**

A: Technically yes, but it's not recommended. Context is meant for human-readable guidance and coordination files (Markdown, JSON, YAML). Binary files won't be useful to agents reading the directory.

---

## Next steps

- Set up your **global context** with coding style and common mistakes to avoid — it travels with you.
- Add **repo context** to projects you work on frequently — it improves over time.
- Use **workflow context** in multi-step workflows to coordinate between steps.
- See [Overlays](09-overlays.md) for syntax reference and configuration options.

[← GitHub Integration](13-github-integration.md) · [Architecture (Detailed) →](architecture.md)
