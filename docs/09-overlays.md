# Overlays

Overlays give agent containers access to host resources — directories, environment variables, and personal skills libraries. Each step in a workflow can define its own overlays, ensuring isolated resource access across the task.

---

## Overview

An overlay "opens a door" from the container into the host, mounting a directory, injecting an environment variable, or making a skill available. Unlike most container configurations, overlays from all sources are **additive** — your global config, per-repo config, `AWMAN_OVERLAYS` environment variable, `--overlay` flags, and per-step overlays all combine into a single final resource list.

Overlays are expressed using a unified syntax that works anywhere: `dir()`, `ssh()`, `env()`, and `skill()` expressions. No matter where you configure them — config file, environment variable, CLI flag, or workflow TOML — the syntax is consistent.

---

## Overlay types

### `dir(host:container[:permission])`

Mounts a host directory into the container at a specified path.

**Syntax:**
```
dir(HOST_PATH:CONTAINER_PATH[:ro|rw])
```

- `HOST_PATH` — absolute path or `~/`-prefixed (expanded to your home directory).
- `CONTAINER_PATH` — absolute path inside the container (or `~/` for the container user's home).
- `permission` — optional; `ro` (read-only, default) or `rw` (read-write).

**Examples:**
```
dir(/var/data:/mnt/data:ro)
dir(~/personal-prompts:/mnt/prompts)
dir(/shared/fixtures:/workspace/fixtures:rw)
```

If the host path does not exist when you launch the session, awman exits with an error before launching any container — this catches typos and missing prerequisites (such as `~/.ssh`) early instead of letting Docker silently auto-create an empty mount source.

### `ssh()`

Mounts your SSH key directory into the container for Git operations and SSH-based tools.

**Syntax:**
```
ssh()
```

This is shorthand that expands to mounting `~/.ssh` from the host as read-only into `~/.ssh` inside the container. It takes no arguments.

**Why:** When your workflow needs to push code to a remote repository or authenticate via SSH, you need access to your private keys. `ssh()` makes this automatic without manual directory wiring.

**Example use case:**
```toml
[[teardown]]
type = "push_branch"
overlays = ["ssh()"]
```

**Security:** SSH keys are always mounted read-only. The agent cannot modify or replace your keys, only use them for authentication.

### `env(VAR_NAME)`

Injects a host environment variable into the agent container's environment.

**Syntax:**
```
env(VAR_NAME)
```

- `VAR_NAME` — the exact name of the environment variable on the host (e.g., `GITHUB_TOKEN`, `ANTHROPIC_API_KEY`).
- Multiple vars are expressed as multiple `env()` calls — not comma-separated inside one call.

**Examples:**
```
env(GITHUB_TOKEN)
env(ANTHROPIC_API_KEY)
```

Multiple vars in different sources (comma-separated in `AWMAN_OVERLAYS`, repeated CLI flags, or multiple array entries):
```
env(GITHUB_TOKEN), env(ANTHROPIC_API_KEY)
```

If the named variable is not set on your host, it is silently absent from the container environment — this is not an error. This lets you list optional variables that may only be set in some contexts (CI vs. local development).

**Example use case:**
```toml
[[teardown]]
type = "create_pull_request"
overlays = ["env(GITHUB_TOKEN)"]
```

### `skill(*)`

Mounts all global awman skills into the agent container.

**Syntax:**
```
skill(*)
```

This makes every skill in `~/.awman/skills/` available as a slash command inside the container.

### `skill(name)`

Mounts a single named skill into the agent container.

**Syntax:**
```
skill(NAME)
```

- `NAME` — the directory name of the skill in `~/.awman/skills/` (e.g., `lint`, `review`).
- Multiple named skills are expressed as multiple `skill()` calls — not comma-separated inside one call.

**Examples:**
```
skill(lint)
skill(review)
skill(fetch)
```

Multiple skills:
```
skill(lint), skill(review)
```

If a named skill does not exist in `~/.awman/skills/`, awman exits with an error before launching the container. This catches typos early rather than silently failing inside the container.

### `context(scope[:permission])`

Grants agent containers access to a persistent context directory on the host and delivers system prompt instructions explaining the context's purpose.

Context overlays combine a **directory mount** (persistent files on the host) with **system prompt injection** (agent-specific delivery of context instructions), unified into a single overlay expression. This allows agents to access both shared knowledge and per-project accumulated context.

**Syntax:**
```
context(SCOPE[:ro|rw])
```

- `SCOPE` — one of `global`, `repo`, or `workflow`:
  - `global` — persistent cross-project developer preferences (`~/.awman/context/global/`)
  - `repo` — project-specific context (`~/.awman/context/repo/{owner}/{repo}/`)
  - `workflow` — per-workflow shared workspace for multi-agent coordination (`~/.awman/context/workflow/`)
- `permission` — optional; `ro` (read-only) or `rw` (read-write, default).

**Examples:**
```
context(global)        # personal preferences, read-write
context(repo)          # project context, read-write
context(workflow:ro)   # workflow workspace, read-only
context(global:rw)     # personal preferences, explicit read-write
```

**What happens when you use `context()`:**

1. awman mounts a directory into the container so the agent can read and (if `rw`) write files there.
2. awman injects a system prompt explaining what the context directory is, how to use it, and what files might be inside.
3. The directory and prompt are automatically managed by awman — you don't need to create them manually.

**Agent system prompt support:**

Most agents natively support system prompt injection. If your agent does not (e.g. `maki`, `crush`), awman still mounts the context directory, but the agent will not be automatically notified via system prompt — you can reference the mounted directory in your prompt manually. For details, see [Context Overlays](14-context-overlays.md).

**Note on `rw` vs `ro`:**

- `context(global)` and `context(repo)` default to `rw` because they are designed for agents to write accumulated knowledge back for future sessions.
- `context(workflow)` is typically `rw` so agents can share state between steps.
- Use `:ro` for read-only access if you want to ensure agents cannot modify the context directory (e.g. in CI/CD or shared environments).

---

## Configuration sources

Overlays can be configured in five places. All sources are merged; the order below shows precedence for conflict resolution (lower priority first):

| Priority | Source | Example |
|----------|--------|---------|
| 1 (lowest) | Global config `~/.awman/config.json` | `"overlays": ["dir(~/shared:/mnt/shared:ro)"]` |
| 2 | Per-repo config `.awman/config.json` | `"overlays": ["skill(lint)"]` |
| 3 | `AWMAN_OVERLAYS` environment variable | `export AWMAN_OVERLAYS="dir(/data:/mnt:ro),env(TOKEN)"` |
| 4 | `--overlay` CLI flags | `--overlay "ssh()" --overlay "env(GITHUB_TOKEN)"` |
| 5 (highest) | Per-step overlays in workflows | `overlays = ["ssh()"]` in TOML |

---

## Merge semantics

All sources are **additive** — they are combined into a single final list. Special rules apply to avoid conflicts:

### Directory overlays

When different sources specify the **same host path**:
- The **higher-priority source** wins for the container path.
- The **more restrictive permission always wins** — `:ro` beats `:rw` regardless of priority. If a lower-priority source declares `:ro`, a higher-priority flag cannot escalate it to `:rw`.

When different sources specify **different host paths** mapping to the **same container path**, both mounts are applied and a warning is logged (Docker will shadow one with the other).

### Skills overlays

Skills use **union/additive** semantics:
- If *any* source specifies `skill(*)`, all skills are mounted.
- Named skills from all sources are accumulated. If global config specifies `skill(foo)` and a per-step overlay specifies `skill(bar)`, both `foo` and `bar` are mounted.
- When `skill(*)` is active from any source, the accumulated named skills list is ignored (all skills are already mounted).

### Environment variable overlays

Environment variable names are deduplicated by name. If multiple sources list the same `env(VAR)`, the final result includes that variable exactly once.

### Context overlays

Context overlays use **union** semantics — all specified scopes are mounted and all prompts are combined. If the same scope appears in multiple sources (e.g. `context(global)` in both global config and a step's overlays), it appears exactly once in the final mount list. The combined system prompt always presents scopes in the order: global, repo, workflow (workflow last).

---

## Global config

**Path:** `~/.awman/config.json`

```json
{
  "overlays": [
    "dir(~/shared-prompts:/mnt/prompts:ro)",
    "env(ANTHROPIC_API_KEY)",
    "skill(*)"
  ]
}
```

This configuration applies overlays to all agent sessions on your machine unless overridden by a per-repo config.

### Table reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `overlays` | string array | `[]` | List of overlay expressions to apply to all sessions. Merged additively with per-repo, `AWMAN_OVERLAYS`, and `--overlay` flags. |

---

## Per-repo config

**Path:** `.awman/config.json` (in the repository root)

```json
{
  "overlays": [
    "skill(lint)",
    "skill(review)",
    "env(GITHUB_TOKEN)"
  ]
}
```

This configuration applies overlays to all agent sessions in this repository only. It merges additively with the global config, so if your global config specifies `skill(*)` and your repo config specifies `skill(lint)`, both are effective (the wildcard mounts everything).

### Table reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `overlays` | string array | `[]` | List of overlay expressions for this repo. Merged additively with global config, `AWMAN_OVERLAYS`, and `--overlay` flags. |

---

## Environment variable

Set `AWMAN_OVERLAYS` in your shell profile to inject overlays without modifying any config file:

```sh
export AWMAN_OVERLAYS="ssh(),dir(~/team-data:/mnt/data:ro),env(GITHUB_TOKEN),skill(fetch)"
```

Expressions are comma-separated. This source has higher priority than config files but lower priority than `--overlay` flags.

---

## CLI flag

The `--overlay` flag is available on all agent-launching commands: `chat`, `exec prompt`, and `exec workflow`. Pass it once per expression:

```sh
awman chat --overlay "ssh()" --overlay "env(GITHUB_TOKEN)"
```

Or comma-separate multiple expressions in a single flag:

```sh
awman chat --overlay "ssh(),env(GITHUB_TOKEN),skill(lint)"
```

This source has the highest priority among global/repo/env/flag sources, but per-step overlays in workflows override it.

---

## Workflow step overlays

In workflow TOML or YAML files, each step can define its own `overlays` array:

**TOML example:**
```toml
[[step]]
name = "research"
prompt = "Research the topic..."
overlays = ["ssh()", "skill(search)", "skill(fetch)"]

[[step]]
name = "write"
prompt = "Write the report..."
overlays = ["dir(/data/reports:/workspace/reports:rw)", "skill(*)"]
```

**YAML example:**
```yaml
steps:
  - name: research
    prompt: "Research the topic..."
    overlays:
      - "ssh()"
      - "skill(search)"
      - "skill(fetch)"
  
  - name: write
    prompt: "Write the report..."
    overlays:
      - "dir(/data/reports:/workspace/reports:rw)"
      - "skill(*)"
```

Per-step overlays merge additively with all other sources (global, repo, `AWMAN_OVERLAYS`, flags), and take highest priority for conflict resolution.

### Workflow-level overlays

Workflows can specify a top-level `overlays` array that applies to every step in the workflow:

**TOML example:**
```toml
[workflow]
title = "Multi-step implementation"

overlays = ["context(repo)", "context(workflow)"]

[[step]]
name = "plan"
prompt = "Plan the implementation..."

[[step]]
name = "implement"
prompt = "Implement according to the plan..."

[[step]]
name = "test"
prompt = "Test the implementation..."
```

**YAML example:**
```yaml
title: Multi-step implementation
overlays:
  - "context(repo)"
  - "context(workflow)"

steps:
  - name: plan
    prompt: "Plan the implementation..."
  
  - name: implement
    prompt: "Implement according to the plan..."
  
  - name: test
    prompt: "Test the implementation..."
```

Workflow-level overlays apply to all steps unless overridden by step-level overlays (which take highest priority). This is useful for context overlays that you want all steps to share (e.g. `context(repo)` so every agent in the workflow has access to the project's accumulated knowledge).

### Setup and teardown steps

Setup and teardown steps (the phases that run before and after workflow steps) also support overlays:

**TOML example:**
```toml
[[setup]]
type = "run_shell"
command = "apt-get update"
overlays = ["dir(/cache:/var/cache:rw)", "env(DEBIAN_FRONTEND)"]

[[teardown]]
type = "push_branch"
overlays = ["ssh()"]

[[teardown]]
type = "create_pull_request"
overlays = ["env(GITHUB_TOKEN)"]
```

Setup and teardown steps support `dir()`, `ssh()`, and `env()` overlays. They do **not** support `skill()` or `skill(*)` (skills are only meaningful for agent containers; setup and teardown are host operations or custom containers).

---

## Common use cases

### Git operations with SSH

When your workflow pushes branches or interacts with SSH-based Git hosts:

```toml
[[teardown]]
type = "push_branch"
overlays = ["ssh()"]
```

The `ssh()` overlay mounts `~/.ssh` into the container, making your private keys available for Git authentication.

### GitHub API operations

When your workflow creates pull requests or interacts with GitHub:

```toml
[[teardown]]
type = "create_pull_request"
overlays = ["env(GITHUB_TOKEN)"]
```

The `env(GITHUB_TOKEN)` overlay injects your GitHub token so the step can authenticate to the GitHub API.

### Anthropic API authentication

When your agent needs an Anthropic API key:

```json
{
  "overlays": ["env(ANTHROPIC_API_KEY)"]
}
```

Set `ANTHROPIC_API_KEY` in your shell environment; awman passes it into the agent container.

### Personal skills library

To make your custom skills available in every session:

**Global config** (`~/.awman/config.json`):
```json
{
  "overlays": ["skill(*)"]
}
```

All skills you've created with `awman new skill` now appear as slash commands in every agent session.

### Personal developer context

To share your personal coding preferences, style rules, and past-mistake notes across all projects:

**Global config** (`~/.awman/config.json`):
```json
{
  "overlays": ["context(global)"]
}
```

Create `~/.awman/context/global/` and add files documenting your preferences (e.g. `coding-style.md`, `common-mistakes.md`). Every agent you run will receive your guidance automatically via system prompt, without needing to duplicate it across project-specific `CLAUDE.md` files.

### Project-specific context

To give agents access to your project's accumulated architecture notes and team knowledge:

**Per-repo config** (`.awman/config.json`):
```json
{
  "overlays": ["context(repo)"]
}
```

awman maintains `~/.awman/context/repo/{owner}/{repo}/` automatically. After each agent session, you can add or update files in this directory (e.g. `architecture.md`, `gotchas.md`). Future agents working on the project will be guided by this accumulated knowledge.

### Selective skills in a workflow

When different steps need different skills:

```toml
[[step]]
name = "lint"
prompt = "Lint the code..."
overlays = ["skill(lint)"]

[[step]]
name = "review"
prompt = "Review the PR..."
overlays = ["skill(review)"]

[[step]]
name = "refactor"
prompt = "Refactor the code..."
overlays = ["skill(lint)", "skill(refactor)"]
```

Each step only mounts the skills it needs, keeping resource access minimal.

### Shared team data

Mount shared reference data or fixture files:

```json
{
  "overlays": [
    "dir(/var/team-data/fixtures:/mnt/fixtures:ro)",
    "dir(~/team-templates:/mnt/templates:ro)"
  ]
}
```

---

## Path expansion

### Host-side `~` expansion

Paths starting with `~/` are expanded to your home directory when the overlay is parsed:

```
dir(~/my-data:/container/data:ro)  →  dir(/home/alice/my-data:/container/data:ro)
ssh()                              →  dir(/home/alice/.ssh:~/.ssh:ro)  [internally]
```

This expansion happens at parse time, so the paths are absolute in all downstream layers.

### Container-side `~/` expansion

Container paths starting with `~/` are expanded at container launch time using the agent's home directory:

```
dir(/host/data:~/data:ro)
```

For root-based containers (the default), `~/` becomes `/root/`. For non-root containers, `~/` becomes `/home/{username}/` where the username comes from the agent's `Dockerfile`. This ensures `ssh()` and other `~/`-based paths land in the correct location for both root and non-root containers.

---

## Error handling

### Malformed expressions

Malformed overlay expressions cause the command to exit immediately with a descriptive error:

```
error: malformed overlay expression (missing colon): "notvalid"
error: 'skill()' requires an argument; use skill(*) to mount all skills or skill(name) for a specific named skill
error: 'env(A, B)' has multiple arguments; use separate env() calls for each variable
error: 'context()' requires a scope; use context(global), context(repo), or context(workflow)
error: 'context(global:rx)' has invalid permission; use 'ro' or 'rw'
```

The command does not proceed — you must fix the syntax before launching.

### Missing host paths

If a configured host path does not exist when you launch the session:

```
warning: overlay host path '/nonexistent/data' does not exist; skipping
```

The warning is logged, but the session proceeds without that overlay. This is intentional — you may list optional paths that only exist in some contexts (CI vs. local, different machines, etc.).

### Missing named skills

If you request a skill that doesn't exist in `~/.awman/skills/`:

```
error: skill 'nonexistent' not found in ~/.awman/skills/
```

The command exits immediately. This catches typos before the container launches, preventing silent failures.

### `AWMAN_OVERLAYS` errors

If `AWMAN_OVERLAYS` contains a malformed expression, the command exits with an error that includes the variable name:

```
error: malformed AWMAN_OVERLAYS: 'skill()' requires an argument (use 'skill(*)' or 'skill(name)')
```

This makes the source immediately obvious so you can fix your shell profile.

### Removed forms

The old config schema used `{ "overlays": { "skills": true, "directories": [...] } }`. This format is no longer supported:

```
error: config overlay format is out of date
```

Update your config to use the new `"overlays": ["skill(*)", "dir(...)"]` format. The old `envPassthrough` field is also deprecated; use `env()` overlay expressions instead.

---

## Workflow source order example

Given a realistic setup:

**Global config** (`~/.awman/config.json`):
```json
{ "overlays": ["dir(~/team-shared:/mnt/shared:ro)", "context(global)"] }
```

**Per-repo config** (`.awman/config.json`):
```json
{ "overlays": ["skill(lint)", "context(repo)"] }
```

**Environment**:
```sh
export AWMAN_OVERLAYS="env(GITHUB_TOKEN)"
```

**CLI invocation**:
```sh
awman exec workflow workflow.toml --overlay "ssh()"
```

**Workflow** (top-level):
```toml
overlays = ["context(workflow)"]

[[step]]
overlays = ["skill(review)"]
```

**Merged result for this step:**
- Directories: `/home/alice/team-shared:/mnt/shared:ro` (from global config)
- Skills: `lint` (from repo config) + `review` (from step)
- Environment: `GITHUB_TOKEN` (from `AWMAN_OVERLAYS`)
- SSH: `~/.ssh:~/.ssh:ro` (from CLI flag)
- Context: `global` (from global config) + `repo` (from repo config) + `workflow` (from workflow level)
- Combined system prompt: global context instructions, then repo context instructions, then workflow context instructions

---

## Security considerations

- All overlay mounts are printed in the full Docker command before each session — you always see exactly what is mounted.
- `:ro` (read-only) prevents the agent from modifying the overlaid directory. Skills are always read-only.
- Only use `:rw` when the task genuinely requires write access to that directory.
- Environment variable overlays are masked in displayed commands (values shown as `***`) to avoid leaking secrets in logs.
- SSH keys are always mounted read-only — the agent cannot modify or replace your keys, only use them for authentication.
- **Context overlays (`rw` default):** Because `context(global)` and `context(repo)` default to read-write, agents can persist files to your host that will be automatically injected into future agent runs. This is intentional — it allows agents to accumulate and refine knowledge. Use `:ro` (e.g. `context(repo:ro)`) if you want read-only context in CI/CD or shared environments. See [Context Overlays](14-context-overlays.md) for more details on managing context.

See [Security & Isolation](04-security-and-isolation.md) for additional details on container transparency and isolation.

---

## Troubleshooting

### "Path does not exist" warning

If you see this warning for a path that should exist, check:
- Is the path spelled correctly?
- Is it an absolute path or properly `~`-expanded?
- Does the host path actually exist? Try `ls -la /path/to/check`.

### Skills not available in container

- Is `skill(*)` or `skill(skillname)` configured?
- Do the skills exist in `~/.awman/skills/`?
- Check the full Docker command printed before the session — it should include `-v` mounts for each skill.

### Git operations failing with "Permission denied"

- Is `ssh()` configured in the step's overlays (or elsewhere)?
- Does `~/.ssh` exist on your host?
- Is the SSH key in `~/.ssh/id_ed25519` or equivalent?

### Environment variable not present in container

- Is `env(VAR_NAME)` configured?
- Is the variable set in your current shell? Try `echo $VAR_NAME`.
- If the variable is unset on the host, it is silently absent from the container — this is not an error, by design.

### Context directory not visible in container

- Is `context(global)`, `context(repo)`, or `context(workflow)` configured?
- Check the full Docker command printed before the session — it should include `-v` mounts for `/awman/context/...`.
- The context directory is auto-created if it doesn't exist. If directory creation fails (permission denied), awman will report an error.

### Agent not receiving context system prompt

- Is your agent one that supports system prompt injection? Check the [Context Overlays](14-context-overlays.md) guide for which agents support native injection.
- For agents that support it (e.g. `claude`), is `context(...)` configured?
- If your agent doesn't support native injection (e.g. `maki`), the context directory is still mounted; reference it manually in your prompt.

### Context files not being updated

- Is the context overlay `rw` (read-write)? By default they are `rw`, but check your config.
- The context directory is writable by agents. Files written by one session will be visible to the next session.
- If you want to prevent agents from modifying context, use `:ro` (e.g. `context(repo:ro)`).

---

[← Configuration](08-configuration.md) · [Next: API Mode →](10-api-mode.md)
