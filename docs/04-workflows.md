# Workflows

A workflow breaks a large implementation task into discrete phases — for example: plan → implement → review → docs. Each phase runs as its own agent session. You review the output between phases and decide whether to advance, retry, or redirect.

Workflows are files you write and commit to your repo — in TOML or YAML format. awman parses them into an execution plan and runs them inside Docker containers, pausing between steps for your input. Optional `setup` and `teardown` sections allow you to prepare the environment before the first step (e.g., installing dependencies, checking out branches) and perform post-workflow actions (e.g., running tests, creating pull requests).

**Migration from Markdown:** Markdown workflow files (`.md`) are no longer supported as of this release. If you have existing Markdown workflows, convert them to TOML or YAML using the format examples below. The conversion is straightforward — all step definitions map directly to TOML/YAML syntax.

---

## When to use workflows

Workflows are useful when:

- The task is complex enough that you want the agent to plan before coding
- You want multiple review checkpoints (e.g. review the plan before implementation starts)
- You want documentation generated as a separate step after implementation
- You're running in `--yolo` mode and want structured auto-advancement instead of a single long session

---

## Quick start

```sh
# Run a workflow file
awman exec workflow aspec/workflows/implement-hard.toml

# Run a workflow and associate a work item for template variable substitution
awman exec workflow aspec/workflows/implement-hard.toml --work-item 0027

# Run a workflow without a work item
awman exec workflow aspec/workflows/dependency-upgrade.toml
```

Use `exec workflow` to run any workflow file. The work item is optional — associate one with `--work-item` if you want template variable substitution. See [API Mode](09-api-mode.md#awman-exec-workflow-path--awman-exec-wf-path) for usage in CI and scripting contexts.

The TUI shows a **workflow status strip** between the execution window and the command box, with one coloured box per step. After each step completes, a confirmation dialog appears — press **Enter** to advance, **q** to pause. State is saved to disk so you can resume later.

---

## Creating a workflow file

Use `awman new workflow` to create a workflow file interactively without having to remember the schema by hand.

### Interactive step entry

```sh
# CLI
awman new workflow

# TUI command box
new workflow
```

Both modes prompt for:

1. **Workflow name** — used as the filename slug (e.g. `my-workflow`). Must contain only letters, digits, hyphens, and underscores.
2. **Workflow title** — a human-readable label that appears at the top of the file (may differ from the name).
3. **Steps** — repeat for each step:
   - Step name (required)
   - Agent (optional — press Enter to skip)
   - Model (optional — press Enter to skip)
   - Depends-on (optional — comma-separated step names, press Enter to skip)
   - Prompt text — enter multiple lines and end with a line containing only `.`

After each step you are asked whether to add another. When finished, awman writes the file and prints its path.

**TUI key bindings** (workflow dialog):

| Key | Action |
|-----|--------|
| **Tab** / **Shift-Tab** | Cycle through fields |
| **Ctrl-N** | Commit the current step and start a new one |
| **Ctrl-Enter** | Finish — write the file and close the dialog |
| **Esc** | Cancel without writing |

By default awman writes to `aspec/workflows/<name>.toml` inside the current repo. Pass `--format` to choose a different format:

```sh
awman new workflow --format yaml   # writes aspec/workflows/<name>.yaml
```

### Interview mode

```sh
awman new workflow --interview
```

Enter a one-paragraph summary of what the workflow should accomplish. A code agent writes the complete workflow file for you — filling in step names, dependencies, agents, models, and detailed prompts — the same way `new spec --interview` writes a work item.

In the TUI, the dialog switches to a two-field layout: workflow name and summary. Press **Ctrl-Enter** to start the interview agent.

### Global workflows

```sh
awman new workflow --global
```

Writes to `~/.awman/workflows/<name>.<ext>` instead of the current repo. Use this to build a personal library of reusable workflows that travel with you across projects.

`--global` and `--interview` can be combined. When combined, the agent is given access only to the `~/.awman/workflows/` directory — not the whole repo or home directory — so your other files stay safe. This still requires being inside a git repository (for agent image lookup).

### Flags

| Flag | Description |
|------|-------------|
| `--interview` | Let a code agent complete the workflow from a short summary |
| `--global` | Write to `~/.awman/workflows/` instead of the current repo |
| `--format <fmt>` | Output format: `toml` (default) or `yaml`. Markdown is not supported |

### Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Name contains spaces or path separators | Rejected immediately with a descriptive error |
| Workflow file already exists | Error with the existing path; awman does not overwrite silently |
| Not inside a git repo (non-global) | Error: run with `--global` to write to `~/.awman/` |
| `--global --interview` outside a git repo | Error: agent image lookup requires a git repo |
| Empty step name in TUI | Inline error; dialog stays open |
| No steps added before Ctrl-Enter (TUI) | Inline error: "At least one step is required" |
| Step prompt is empty (CLI) | Warning logged; empty prompt written to file |
| `depends_on` names non-existent steps | Warning logged; file is still written (steps may be added later) |
| Load a `.md` workflow file | Error: "Markdown workflow files are no longer supported. Convert to TOML (.toml) or YAML (.yaml/.yml). See docs/04-workflows.md for the current format." |

---

## Workflow file formats

awman supports two workflow file formats: **TOML** (`.toml`) and **YAML** (`.yml` / `.yaml`). The format is detected automatically from the file extension. Both formats produce identical execution behaviour — you can pass either to `--workflow` interchangeably.

| Extension | Format |
|-----------|--------|
| `.toml` | TOML |
| `.yml` or `.yaml` | YAML |

Any other extension is rejected with:

```
unsupported workflow format: expected .toml, .yml, or .yaml
```

### Step fields

All steps support the same fields:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique step identifier within the workflow |
| `prompt` | string | yes | Prompt template sent to the agent |
| `depends_on` | array of strings | no | Names of steps that must complete before this one runs |
| `agent` | string | no | Run this step with a specific agent instead of the default. Valid values: `claude`, `codex`, `opencode`, `maki`, `gemini` |
| `model` | string | no | Run this step with a specific model. Overrides any `--model` flag |

Field names are **lowercase only** (`name`, `depends_on`, `agent`, `model`, `prompt`). Uppercase variants are not accepted. Unknown fields (e.g. `dependson`, `Prompt`) are rejected as errors so that typos do not silently take effect.

### TOML (`.toml`)

Steps are declared as an array of tables using `[[step]]`. The optional `title` string appears at the top level.

```toml
title = "Implement Feature Workflow"   # optional

[[step]]
name = "plan"
prompt = """
Read the following work item and produce an implementation plan.

{{work_item_content}}
"""

[[step]]
name = "implement"
depends_on = ["plan"]
prompt = """
Implement work item {{work_item_number}} according to the plan.

Follow the spec: {{work_item_section:[Implementation Details]}}
"""

[[step]]
name = "review"
depends_on = ["implement"]
prompt = """
Review the changes from the implement step for correctness and style.
"""

[[step]]
name = "docs"
depends_on = ["implement"]
prompt = """
Write documentation for work item {{work_item_number}}.
"""
```

Use TOML triple-quoted strings (`"""…"""`) for multiline prompts. Newlines and `{{template_vars}}` are preserved exactly.

### YAML (`.yml` / `.yaml`)

Steps are declared as a sequence under the `steps` key. The optional `title` string appears at the top level.

```yaml
title: "Implement Feature Workflow"   # optional

steps:
  - name: plan
    prompt: |
      Read the following work item and produce an implementation plan.

      {{work_item_content}}

  - name: implement
    depends_on: [plan]
    prompt: |
      Implement work item {{work_item_number}} according to the plan.

      Follow the spec: {{work_item_section:[Implementation Details]}}

  - name: review
    depends_on: [implement]
    prompt: |
      Review the changes from the implement step for correctness and style.

  - name: docs
    depends_on: [implement]
    prompt: |
      Write documentation for work item {{work_item_number}}.
```

Use YAML literal blocks (`|`) for multiline prompts. `depends_on` must be a YAML sequence — not a bare string. Newlines and `{{template_vars}}` are preserved exactly.

---

## Setup and teardown phases

Workflows can include optional `setup` and `teardown` sections to prepare the environment before the main steps and perform post-workflow actions.

**Setup phase** runs before the first main step and is intended for:
- Checking out or creating a Git branch
- Pulling latest changes
- Installing dependencies
- Running build or configuration scripts
- Cloning additional repositories needed by the workflow

**Teardown phase** runs after all main steps complete (or on failure, if `teardown_on_failure` is enabled) and is intended for:
- Running tests
- Committing changes
- Creating pull requests
- Pushing branches to a remote
- Cleanup operations

All setup and teardown steps execute inside the project's **base container image** — the same isolated Docker container used for agent steps. No shell commands are ever executed directly on the host. Each phase uses its own container instance: a setup container runs all setup steps, then is killed; later, a teardown container is started for all teardown steps.

### Setup step types

Setup steps are defined in a `[[setup]]` (TOML) or `setup:` (YAML) array. Each step has a `type` field and type-specific fields:

| Type | Fields | Description |
|------|--------|-------------|
| `clone_repo` | `url` (string, required), `branch` (string, optional), `into` (string, optional) | Clone a repository. `branch` checks out a specific branch. `into` specifies the target directory (relative to workdir); omit to use the repo name. Useful for cloning additional repos needed by the workflow (for the primary repo, use the session's `repo_url` and `branch` fields instead). |
| `checkout_create_branch` | `branch` (string, required), `base` (string, optional) | Check out an existing branch or create a new one. If `base` is specified, the branch is created from that ref. Attempts to fetch from the remote first; if unavailable or not configured, falls back to local creation. |
| `pull_branch` | `remote` (string, optional), `branch` (string, optional) | Pull the latest changes from a remote branch. Equivalent to `git pull <remote> <branch>`. Omit both to use `git pull` with defaults. |
| `run_shell` | `command` (string, required), `env` (object, optional) | Execute a shell command. `env` is an optional object of environment variables to inject (`{"KEY": "value"}`). |
| `run_script` | `path` (string, required), `env` (object, optional) | Execute a shell script file (relative to the workdir). `env` is an optional object of environment variables. |

Example TOML setup:

```toml
[[setup]]
type = "checkout_create_branch"
branch = "feature/my-feature"
base = "main"

[[setup]]
type = "run_shell"
command = "npm install"

[[setup]]
type = "run_shell"
command = "npm run build"
```

Example YAML setup:

```yaml
setup:
  - type: checkout_create_branch
    branch: feature/my-feature
    base: main
  - type: run_shell
    command: npm install
  - type: run_shell
    command: npm run build
```

### Teardown step types

Teardown steps are defined in a `[[teardown]]` (TOML) or `teardown:` (YAML) array. Each step has a `type` field and type-specific fields:

| Type | Fields | Description |
|------|--------|-------------|
| `run_shell` | `command` (string, required), `env` (object, optional) | Execute a shell command. |
| `run_script` | `path` (string, required), `env` (object, optional) | Execute a shell script file. |
| `commit_changes` | `message` (string, required), `add_all` (boolean, optional) | Commit staged changes. If `add_all` is `true`, runs `git add -A` first. |
| `push_branch` | `remote` (string, optional), `branch` (string, optional) | Push the current branch to a remote. Omit both to use `git push` with defaults. |
| `create_pull_request` | `title` (string, optional), `body` (string, optional), `base` (string, optional) | Create a pull request using the GitHub CLI. If `base` is provided, it sets the branch the PR will be opened against (via the `--base` flag). Requires `gh` to be available in the base container image. |

Example TOML teardown:

```toml
[[teardown]]
type = "run_shell"
command = "npm test"

[[teardown]]
type = "commit_changes"
message = "automated changes"
add_all = true

[[teardown]]
type = "create_pull_request"
title = "feat: automated implementation"
body = "This PR was created automatically by an awman workflow."
base = "main"
```

Example YAML teardown:

```yaml
teardown:
  - type: run_shell
    command: npm test
  - type: commit_changes
    message: "automated changes"
    add_all: true
  - type: create_pull_request
    title: "feat: automated implementation"
    body: "This PR was created automatically by an awman workflow."
    base: main
```

### Teardown on failure

By default, teardown steps are skipped if the workflow fails. To run teardown even on failure (e.g., to clean up partial artifacts), set `teardown_on_failure = true` at the top level:

```toml
name = "implement-feature"
teardown_on_failure = true

# steps, setup, teardown defined below...
```

If any teardown step fails, the error is logged and execution continues to the next teardown step (best-effort cleanup). Teardown failure does not retroactively change the workflow's success/failure status.

### Container execution model

**Setup container lifecycle:**
1. Before setup runs, awman starts a background container from the base image with the session workdir mounted
2. Each setup step is executed via `exec` into the running container
3. After all setup steps complete (or if any step fails), the setup container is killed
4. If setup fails, the main workflow steps do not run

**Teardown container lifecycle:**
1. After all main steps complete, awman starts a fresh teardown container (separate from the setup container)
2. Each teardown step is executed via `exec` into the teardown container
3. After all teardown steps complete, the teardown container is killed
4. If `teardown_on_failure = false` and the workflow failed, teardown is skipped entirely

All environment variables configured for the project (via overlays, config, or per-step `env` fields) are inherited by both setup and teardown containers, just as they are for main workflow steps.

### Remote sessions and setup/teardown

For `type: remote` API sessions, the repository is already cloned and the branch is checked out by awman at session creation time — the session's working directory points to a fresh clone before any setup steps begin. A `clone_repo` setup step is therefore redundant for provisioning the primary repo and should not be used for that purpose. It remains valid for cloning *additional* repositories needed by the workflow into subdirectories.

### Idempotency

If a workflow is interrupted mid-setup or mid-teardown and then resumed, the full setup or teardown phase is re-run from the beginning. Setup steps should be written to be idempotent — i.e., they should succeed whether or not they have been run before:

- Use `git clone <url> || true` if the directory might already exist
- Use `checkout_create_branch` instead of raw `git checkout -b` (it handles existing branches automatically)
- Ensure package manager commands (`npm install`, `pip install`) are idempotent
- For custom scripts, make them idempotent by checking preconditions

### Base image and `gh` CLI

If your teardown includes a `create_pull_request` step, the base container image **must have the GitHub CLI (`gh`) installed**. By default, the base image is the project's `Dockerfile.dev`. If `gh` is not available, the step will fail with a clear error. Update your `Dockerfile.dev` to include `gh` if you intend to use `create_pull_request` steps.

### Complete example: setup + steps + teardown

This example shows a full workflow that creates a branch, installs dependencies, runs implementation and review steps, then commits, pushes, and opens a PR:

```toml
title = "Implement and Ship"
teardown_on_failure = false

[[setup]]
type = "checkout_create_branch"
branch = "feature/{{work_item_number}}"
base = "main"

[[setup]]
type = "run_shell"
command = "cargo fetch"

[[step]]
name = "implement"
model = "claude-opus-4-6"
prompt = """
Implement work item {{work_item_number}} according to the spec.
"""

[[step]]
name = "review"
depends_on = ["implement"]
prompt = """
Review the changes for correctness and style.
"""

[[teardown]]
type = "run_shell"
command = "make test"

[[teardown]]
type = "commit_changes"
message = "Implement {{work_item_number}}"
add_all = true

[[teardown]]
type = "push_branch"

[[teardown]]
type = "create_pull_request"
title = "Implement {{work_item_number}}"
body = "Automated PR from awman workflow."
base = "main"
```

The equivalent YAML:

```yaml
title: Implement and Ship
teardown_on_failure: false

setup:
  - type: checkout_create_branch
    branch: "feature/{{work_item_number}}"
    base: main
  - type: run_shell
    command: cargo fetch

steps:
  - name: implement
    model: claude-opus-4-6
    prompt: |
      Implement work item {{work_item_number}} according to the spec.
  - name: review
    depends_on: [implement]
    prompt: |
      Review the changes for correctness and style.

teardown:
  - type: run_shell
    command: make test
  - type: commit_changes
    message: "Implement {{work_item_number}}"
    add_all: true
  - type: push_branch
  - type: create_pull_request
    title: "Implement {{work_item_number}}"
    body: Automated PR from awman workflow.
    base: main
```

### Template variables

Template variables are available in **all workflow fields** — step `prompt` values, setup step fields, and teardown step fields. The only field that does not support substitution is `type` (which selects the step kind).

| Variable | Replaced with |
|----------|--------------|
| `{{work_item_number}}` | Zero-padded four-digit work item number (e.g. `0027`) |
| `{{work_item}}` | Bare numeric work item number (e.g. `27`) |
| `{{work_item_content}}` | Full text of the work item Markdown file |
| `{{work_item_section:[Name]}}` | Content of the named `## Name` section from the work item file (case-insensitive heading match, trailing colons stripped) |

All variables require `--work-item` to be passed when running the workflow. If `--work-item` is omitted, `{{work_item_*}}` placeholders are replaced with empty strings and a warning is emitted.

Unknown variables or missing sections are left in place with a warning.

**Examples across workflow phases:**

```toml
# Setup — branch name derived from work item
[[setup]]
type = "checkout_create_branch"
branch = "feature/{{work_item_number}}"

# Step — prompt references work item content
[[step]]
name = "implement"
prompt = "Implement {{work_item_number}}: {{work_item_section:[Summary]}}"

# Teardown — commit message and PR title include work item number
[[teardown]]
type = "commit_changes"
message = "Implement {{work_item_number}}"
add_all = true

[[teardown]]
type = "create_pull_request"
title = "feat: {{work_item_number}}"
body = "{{work_item_section:[Summary]}}"
```

---

## Multi-agent workflows

Each step in a workflow can run in a different agent's container by adding an `agent` key to the step.

```toml
[[step]]
name = "plan"
prompt = "Produce an implementation plan."

[[step]]
name = "implement"
depends_on = ["plan"]
agent = "codex"
prompt = "Implement the plan from the previous step."

[[step]]
name = "review"
depends_on = ["implement"]
agent = "claude"
prompt = "Review the implementation for correctness and style."
```

```yaml
steps:
  - name: plan
    prompt: Produce an implementation plan.
  - name: implement
    depends_on: [plan]
    agent: codex
    prompt: Implement the plan from the previous step.
  - name: review
    depends_on: [implement]
    agent: claude
    prompt: Review the implementation for correctness and style.
```

Steps without an `agent` field use the workflow default agent — the value from repo config (or global config), overridden by the `--agent` flag if one was passed at the command line. The `--agent` flag sets the **default** for steps that do not name an agent; it does **not** override steps that explicitly specify one.

### Agent pre-flight check

Before the first step runs, awman collects every distinct agent name required across all steps and checks that the corresponding image exists. If an image is missing, awman prompts:

```
Agent 'codex' has no Dockerfile. Download and build it? [y/N]:
```

**Accept** — awman downloads the agent Dockerfile template, builds the project base image (if needed), then builds the agent image. If your repo has multiple agents to set up, each is prompted in turn before the workflow begins.

**Decline** — awman asks whether to substitute the default agent for that step instead:

```
Use the default agent (claude) for steps that specify 'codex'? [y/N]:
```

- Accept the fallback: those steps run with the default agent. The workflow starts normally.
- Decline the fallback: the workflow does not start.

If all required images are already available, the pre-flight check completes silently and the first step launches immediately.

### Unknown agents

An unknown agent name in an `agent` field is caught at parse time, before any container runs, and exits with a list of valid options.

### Resuming workflows with per-step agents

When resuming a saved workflow, the per-step agent assignments from the original run are preserved in the state file. If you pass a different `--agent` flag on resume, awman warns you; the persisted assignments still take precedence.

---

## Per-step model overrides

Each step in a workflow can run against a different model by adding a `model` key to the step.

```toml
[[step]]
name = "plan"
agent = "claude"
model = "claude-opus-4-6"
prompt = "Produce a detailed implementation plan."

[[step]]
name = "implement"
depends_on = ["plan"]
agent = "claude"
model = "claude-haiku-4-5"
prompt = "Implement the plan from the previous step."

[[step]]
name = "review"
depends_on = ["implement"]
prompt = "Review the implementation for correctness and style."
```

```yaml
steps:
  - name: plan
    agent: claude
    model: claude-opus-4-6
    prompt: Produce a detailed implementation plan.
  - name: implement
    depends_on: [plan]
    agent: claude
    model: claude-haiku-4-5
    prompt: Implement the plan from the previous step.
  - name: review
    depends_on: [implement]
    prompt: Review the implementation for correctness and style.
```

In this example, `plan` uses a large model for deep reasoning, `implement` uses a smaller model for routine code generation, and `review` inherits whatever model is in effect from the `--model` flag (or the agent's built-in default if no flag was passed).

### Model resolution order

For each step, awman resolves the effective model using this priority:

| Priority | Source | Applies when |
|----------|--------|-------------|
| 1 (highest) | Step's `model` field | The step explicitly declares a model |
| 2 | `--model` flag on the command line | The step has no `model` field |
| 3 (lowest) | Agent built-in default | Neither a step field nor a flag was provided |

The `--model` flag acts as the **default** for all steps without a `model` field; it does **not** override steps that declare their own model.

`agent` and `model` are independent overrides. A step can specify one, both, or neither. When both are present, awman resolves the agent first, then resolves the model.

### Workflow resume and model persistence

Per-step `model` values are persisted in the workflow state file. On resume, the persisted model is used, not any `--model` flag passed on the resumed invocation. This matches the existing behaviour for `agent` fields and ensures the resumed run is identical to the original.

---

## Running a workflow

### In the TUI

```
exec workflow aspec/workflows/implement-hard.toml --work-item 0027
```

A **workflow status strip** appears, showing each step as a coloured box:

| Colour | Status |
|--------|--------|
| Grey / dim | Pending |
| Blue / bold | Running |
| Green | Done |
| Red / bold | Error |
| Yellow / bold | Stuck (idle for >10 s) |

When a step completes, a confirmation dialog appears. Press **Enter** or **y** to advance, **q** or **Esc** to pause.

### In command mode

```sh
awman exec workflow aspec/workflows/implement-hard.toml --work-item 0027
```

Between steps, awman prints the step summary and prompts:

```
Step 'plan' completed.
Next step(s): implement
Press [Enter] to advance, or [q] to abort:
```

On agent failure:

```
Step 'implement' failed: Container exited with code 1
Press [r] to retry, or any other key to abort:
```

### Flags

`exec workflow` accepts the following flags:

| Flag | Description |
|------|-------------|
| `--agent=<name>` | Default agent for steps that do not specify an `agent` field. Does not override steps with an explicit `agent` value |
| `--model=<NAME>` | Default model for steps that do not specify a `model` field. Does not override steps with an explicit `model` value |
| `--non-interactive` | Run each step's agent in print/batch mode |
| `--plan` | Run each step in read-only mode |
| `--allow-docker` | Mount Docker socket into each step's container |
| `--worktree` | Run all steps in an isolated Git worktree |
| `--mount-ssh` | Mount `~/.ssh` into each step's container |
| `--yolo` | Fully autonomous mode; implies `--worktree`; auto-advances stuck steps |

---

## Workflow control board (TUI only)

Press **Ctrl+W** at any time to open the **workflow control board** — a popup that lets you redirect execution without waiting for the current step to finish. Ctrl+W works regardless of whether the container window is maximized or minimized.

There are two variants of the control board:

### Lightweight step confirmation (between steps)

When a step completes and the next step is ready, awman shows a compact confirmation dialog:

```
╭─ Step 'implement' done. Advance to 'test'? ─╮
│                                             │
│  [Enter] yes  [Esc] pause  [Ctrl+W] details │
╰─────────────────────────────────────────────╯
```

| Key | Action |
|-----|--------|
| **Enter** | Advance to the next step |
| **Esc** | Pause and wait for your input |
| **Ctrl+W** | Open the full workflow control board for more options |

### Full workflow control board (between or during steps)

The full control board appears when you have multiple options or want fine-grained control. It can be opened mid-step without disrupting the running container:

```
╭───── Workflow Control ──────╮
│ Step: implement             │
│                             │
│    ↑ Restart current step   │
│                             │
│ ← Prev   → Next (new cont.) │
│                             │
│    ↓ Next (same container)  │
│                             │
│ [Arrow] select  [Esc] done  │
╰─────────────────────────────╯
```

#### Between-step actions

| Key | Effect | Container killed? |
|-----|--------|-------------------|
| **↑** | Restart current step — reset to Pending and relaunch in a fresh container | ✓ Yes |
| **←** | Cancel to previous step — mark current step Pending and re-run the most recently completed step | ✓ Yes |
| **→** | Next step: new container — mark current step Done and advance in a new container | ✓ Yes |
| **↓** | Next step: same container — mark current step Done and send the next step's prompt to the existing container via PTY | ✗ No |
| **Esc** | Dismiss and continue waiting | ✗ No |

#### Mid-step actions (when step is running)

When you open the control board **while a step is actively running**, the same actions are available, but with different implications:

| Key | Effect | Container killed? | Step status |
|-----|--------|-------------------|-------------|
| **→** | Force advance — mark current step Done regardless of completion and launch the next step | ✓ Yes | Treated as succeeded |
| **↓** | Continue in current container — queue a message for the running agent to process | ✗ No | Continues running |
| **Esc** | Dismiss — let the step continue running undisturbed | ✗ No | Continues running |
| **↑**, **←** | (same as between-step) | ✓ Yes | (same as between-step) |

The dialog title shows `Workflow Control (step running)` when opened mid-step. Actions that kill the container display a sub-note in gray: `↳ kills running container`. The dismiss action shows: `↳ step keeps running`.

### Next step: same container

The **↓** action reuses the already-running container — the next step's prompt is written directly to its PTY stdin. Useful when the container has already installed dependencies or built artifacts that the next step needs. If the PTY session has closed, awman falls back to a new container and shows a status message.

If the next step requires a **different agent** than the current step, the **↓** option is unavailable. In the TUI it renders greyed out with the message:

```
Next step uses agent 'codex'; cannot reuse current 'claude' container.
```

In command mode, the "same container" prompt is skipped entirely and the explanation is printed instead. Use **→** (new container) to advance, which always works regardless of agent.

### Manual vs. automatic opening

Ctrl+W works at any time when a workflow is active in the current tab — there are no other preconditions. It works mid-step, between steps, during a yolo countdown, or while another dialog is open (the existing dialog is dismissed first).

---

## Workflow strip and step status

The **workflow status strip** shows the state of every step in the workflow:

```
Running: plan     ┃  ● implement    ✓ review    ⚠️ docs
```

| Visual | Meaning |
|--------|---------|
| **●** (Blue, bold) | Step is currently running |
| **✓** (Green) | Step completed successfully |
| **⚠️** (Yellow, bold) | Step is stuck (no output for >30 seconds) |
| **●** (Gray, dim) | Step is pending |
| **✗** (Red, bold) | Step encountered an error |

### Stuck steps

When a step produces no output for more than 30 seconds, it is marked as stuck in the strip. Stuck steps show a warning indicator (⚠️) both in the strip box and in the tab label.

Stuck steps trigger automatic behavior depending on the mode:
- In **yolo mode**: the engine starts a 60-second countdown. When it expires, the step is auto-advanced. If the user cancels (Esc) and the step re-stucks, the countdown restarts from 60 seconds with no backoff.
- In **non-yolo mode**: the workflow control board opens automatically so you can decide what to do.
- In either mode, new PTY output immediately clears the stuck state and cancels any active countdown.

You can always open the control board manually via **Ctrl+W** regardless of stuck status.

### Parallel step groups

Steps that share the same dependencies form a **parallel group** and execute sequentially in file order. In the workflow strip, they are stacked vertically with slight indentation. If a group has more than two steps, the additional steps are shown as `+ N more…`. Use **mouse wheel** to scroll within the strip and view hidden parallel steps.

### Viewing the full control board

When a step completes, awman shows the lightweight confirmation dialog. To see all available actions and options, press **Ctrl+W** to open the full control board. Pressing **Esc** on the lightweight dialog pauses the workflow for manual input.

---

## Auto-advance when stuck (yolo mode)

When a running workflow step produces no output for **30 seconds**, the engine is notified that the step is stuck:

- In **yolo mode**: the engine starts a 60-second countdown. If the countdown expires, the step is automatically advanced. Pressing Esc cancels the countdown; if the step re-stucks, the countdown restarts from 60 seconds with no backoff.
- In **non-yolo mode**: the workflow control board opens automatically so you can decide what to do.

Stuck detection fires independently per tab — background tabs detect and report stuck state to their own engine. In yolo mode, background tabs show a live countdown in the tab bar. See [Yolo Mode — Background yolo countdown](05-yolo-mode.md#background-yolo-countdown).

**Active-tab suppression:** If you are actively pressing keys or scrolling on the currently active tab, the stuck timer is held back even if the container is silent. The timer starts only once both the container and the user have been idle for 30 seconds. Background tabs are always checked using output time alone.

---

## Workflow state persistence

awman saves workflow state to:

```
$GITROOT/.awman/workflows/<repo-hash8>-<work-item>-<workflow-name>.json
```

The file records the status of every step, the container ID used for each step, and a SHA-256 hash of the workflow file.

### Resuming

If a saved state file exists when you run `exec workflow`, awman offers to resume:

```
Found a saved workflow state for 'implement-feature' (work item 0027).
  1) Resume from where you left off
  2) Restart from the beginning
  [1/2]:
```

### Workflow file changed

If the workflow file has been modified since the state was saved, awman warns you:

```
WARNING: The workflow file has changed since the last run.
  1) Restart from the beginning
  2) Continue anyway (could be dangerous)
  [1/2]:
```

If you choose `2`, awman verifies that step names and `Depends-on` values are identical. If they differ, it forces a restart.

### Interrupted steps

If a step was running when awman last exited:

```
Step 'implement' was running when the previous session ended.
Start it over (s) or skip to next step (n)? [s/n]:
```

---

## Parallel groups

Steps that share the same `Depends-on` set form a **parallel group**. awman executes them sequentially in file order (true parallel container execution is a future enhancement). In the TUI they are rendered stacked vertically. If a group has more than two steps, the third box shows `+ N more…`.

---

## Bundled examples

`aspec/workflows/` contains ready-to-use workflow files:

| File | Description |
|------|-------------|
| `implement-hard.toml` | Four-step workflow: implement → tests + docs (parallel) → review. Uses Opus for implementation, Haiku for docs, and a final interactive review step |
| `implement-pr.toml` | Same four steps as `implement-hard.toml`, plus teardown steps that run tests, commit changes, push the branch, and create a pull request |
| `hard-parity-local.toml` | Five-step workflow: implement → parity check → tests + docs (parallel) → review. Uses a local model via OpenCode for parity checking, tests, and docs |
| `dependency-upgrade.toml` | Two-step workflow: security audit → version audit. Upgrades vulnerable dependencies first, then reviews available version updates |

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Cycle in `depends_on` graph | Error before any agent runs |
| Unknown `depends_on` step name | Error at parse time |
| Unknown agent name in `agent` field | Error at parse time, before any containers run |
| Missing agent image at workflow start | Pre-flight prompt: build it, fall back to default, or abort |
| Agent Dockerfile download fails during pre-flight | Error surfaced; workflow does not start |
| Agent image build fails during pre-flight | Error surfaced; partial Dockerfile removed; workflow does not start |
| `--agent` flag + step with explicit `agent` field | Step's `agent` value wins; `--agent` is only the default for unspecified steps |
| `--model` flag + step with explicit `model` field | Step's `model` value wins; `--model` is only the default for steps without a `model` field |
| `model` combined with `agent` in the same step | Independent overrides; agent resolved first, then model |
| `model` field with no value | Treated as absent; agent launches with its built-in default or `--model` flag value |
| Invalid model name in `model` field | Passed verbatim to the agent; the agent surfaces its own error |
| Resume with a different `--model` flag | Persisted per-step model values take precedence; `--model` applies only to steps with no persisted model |
| All steps specify non-default agents | Pre-flight still runs for each; default fallback offered only if setup is declined |
| Parallel steps with different agents | Each step runs in its own container — no cross-step sharing |
| Resume with a different `--agent` flag | Warning printed; persisted per-step agent assignments take precedence |
| Current step and next step use the same agent | "Same container" (**↓**) option available as usual |
| Current step and next step use different agents | "Same container" option greyed out (TUI) or skipped (CLI) with explanation |
| Empty workflow file | Rejected with a helpful message |
| Unsupported file extension (e.g. `.json`) | Error: `unsupported workflow format: expected .toml, .yml, or .yaml` |
| Markdown workflow file (`.md`) | Error: `Markdown workflow files are no longer supported. Convert to TOML (.toml) or YAML (.yaml/.yml).` |
| TOML/YAML step missing `name` field | Parse error including the step index |
| TOML/YAML step missing `prompt` field | Parse error including the step name (or index if unnamed) |
| Empty `[[step]]` / `steps:` array | Error: `"workflow file contains no steps"` |
| `depends_on` as bare YAML string instead of sequence | Parse error; must be a YAML sequence |
| Unknown field in TOML/YAML step (e.g. `dependson`) | Parse error; typos are not silently dropped |
| Uppercase field name in TOML/YAML (e.g. `Name:`) | Parse error; field names must be lowercase |
| Setup step with invalid type | Parse error; type must be one of the supported step types |
| Teardown step with invalid type | Parse error; type must be one of the supported step types |
| `create_pull_request` step but `gh` not in base image | Step fails at execution time with "command not found: gh" |
| Setup failure | Main workflow steps do not run; go directly to teardown (if `teardown_on_failure = true`) or exit |
| Teardown step failure (non-zero exit) | Error is logged; execution continues to next teardown step (best-effort) |
| `checkout_create_branch` with no remote configured | Falls back to local branch creation from HEAD or specified `base` |
| `run_script` step with non-existent path | Step fails with file-not-found error |
| Setup interrupted and resumed | Full setup phase re-runs from the beginning; steps should be idempotent |
| Work item file not found | Error before loading the workflow |
| Workflow file not found / unreadable | Clear error with the file path |
| Agent failure mid-workflow | Step marked Error; user prompted to retry or abort |
| Very long step names | Truncated to 12 characters with `…` in the TUI strip |
| Large number of parallel steps | Capped at 3 visible rows; extra shown as `+ N more…` |
| Large number of sequential steps | `+ N more…` box at the far right of the strip |
| **d** pressed; auto-popup suppressed | Auto-open skipped until workflow advances; Ctrl+W still works |
| Container window maximized (auto-open) | Dialog opens over the maximized terminal; input routes to dialog |
| Another dialog already open | Both Ctrl+W and auto-open suppressed until open dialog is dismissed |
| Step silent on a background tab (non-yolo) | Auto-open deferred; control board appears when you switch to that tab |
| Step silent on a background tab (yolo) | Live countdown shown in tab bar; dialog opens when you switch to the tab; workflow auto-advances when countdown expires |
| Esc dismissed; container still silent | Timer resets; dialog re-opens after another 10 s |
| Output resumes before 10 s threshold | Stuck state clears; auto-open does not trigger |
| User actively scrolling on active tab | Stuck timer suppressed; control board does not open while user is engaged |
| User becomes idle after scrolling | Timer starts from idle moment; control board opens after another 10 s of silence |

### Limitations (v0.3)

- **Sequential only**: parallel groups run one step at a time. True concurrent container execution is not yet supported.
- **TUI resume dialogs**: hash-mismatch and resume prompts use auto-restart behaviour rather than a full dialog.

---

[← Security & Isolation](03-security-and-isolation.md) · [Next: Yolo Mode →](05-yolo-mode.md)
