# Dynamic Workflows

Dynamic workflows let you run `exec workflow --dynamic` without writing a workflow file yourself. Instead of authoring a `.toml` or `.yaml` file by hand, a "leader" agent is launched inside a container to design a purpose-built `workflow.toml` tailored to your work item. Once the leader finishes, awman validates its output and immediately executes the generated workflow — all in one command.

---

## When to use

Dynamic workflows are useful when:

- You have a work item spec and want awman to figure out the right set of steps without manual workflow authoring
- You want a custom workflow per work item rather than reusing a generic template
- You're running in fully autonomous mode and want end-to-end execution from a single command

Dynamic mode always implies `--yolo`, `--worktree`, and a `context(workflow)` overlay. This means all agent work happens in an isolated Git worktree, and the leader and workflow steps share a context directory for coordination.

---

## Quick start

```sh
# Let awman design and run a workflow for work item 42
awman exec workflow --dynamic --work-item 42

# Same, but use a more capable leader agent
awman exec workflow --dynamic --work-item 42 --leader claude::claude-opus-4-8
```

`--work-item` is required with `--dynamic`. Without it, awman cannot provide the leader agent with the work item it is designing the workflow for.

---

## How it works

When you run `exec workflow --dynamic`, awman follows these steps:

1. **Validate flags** — checks for conflicts before doing any work (see [Flag rules](#flag-rules))
2. **Read the work item** — loads the work item file; fails immediately if it cannot be found or read
3. **Set up the worktree** — creates an isolated Git worktree (the same path as `--yolo --worktree`); all subsequent agent work runs inside it
4. **Seed the context directory** — writes `example-workflow.toml` and `workflow-usage.md` into the `context(workflow)` directory so the leader has reference material
5. **Launch the leader agent** — starts a container with a pre-built prompt describing the work item, available agents, and path conventions; the agent's job is to write a `workflow.toml` to the context directory
6. **Wait for the leader to finish** — stuck detection runs normally; after 30 seconds of silence the 60-second yolo countdown starts; when it expires (or you press `[n]`), the container is killed and awman reads the file
7. **Validate the workflow** — checks that `workflow.toml` is valid TOML, references only agents that exist in the project, and has all required fields; if validation fails, a repair agent is launched (see [Repair loop](#repair-loop))
8. **Build missing images** — for any agent referenced in the generated workflow that has a `Dockerfile.<agent>` but no built image, awman builds the image automatically before starting the workflow
9. **Execute the workflow** — runs the generated workflow exactly as if you had written it yourself and passed it to `exec workflow --yolo --work-item 42`

The leader agent cannot see or modify source files beyond the context directory — the worktree is mounted read-only when the runtime supports it, and any source-file modification by the leader aborts the dynamic run before the generated workflow is executed.

---

## Choosing the leader agent

By default, the leader uses whatever agent is configured as the project default (from `.awman/config.json` or `~/.awman/config.json`). You can override this in two ways:

**`--leader agent::model`** — specify both the agent container and model for the leader:

```sh
awman exec workflow --dynamic --work-item 42 --leader claude::claude-opus-4-8
```

**`--model`** — use the default agent, but with a different model:

```sh
awman exec workflow --dynamic --work-item 42 --model claude-opus-4-8
```

When both `--leader` and `--model` are passed, `--leader` governs the leader agent entirely; `--model` still applies as the session-level default model for steps in the generated workflow.

The `--leader` value must be in `agent::model` format — exactly two components separated by `::`. Examples:

```
claude::claude-opus-4-8       # valid
claude::claude-sonnet-4-6     # valid
claude                        # invalid — missing ::model
::claude-opus-4-8             # invalid — missing agent
claude::opus::extra           # invalid — too many components
```

---

## The leader step

The leader agent runs like a regular workflow step:

- **Stuck detection** fires after 30 seconds of inactivity
- **60-second yolo countdown** starts automatically (dynamic mode always enforces `--yolo`)
- **Auto-advance** kills the container when the countdown expires and reads the generated file
- **`[n] now`** (CLI) or `→` (WCB) advances immediately without waiting for the countdown
- **Ctrl+W** opens the Workflow Control Board at any time

In the Workflow Control Board, the right-arrow action is labelled **"Start dynamic workflow"** instead of the usual "Next: new container", reflecting that advancing past the leader step starts the generated workflow.

Other WCB actions work identically to a regular step:

| Action | Effect |
|--------|--------|
| **↑ Restart** | Kill the leader container, delete the current `workflow.toml`, and re-launch a fresh leader with the same prompt |
| **→ Start dynamic workflow** | Kill the leader container and proceed to validate and execute the generated file |
| **Ctrl+C / [a] Abort** | Kill the leader container and abort the entire dynamic run — no workflow is executed |
| **[p] Pause** | Kill the leader container and pause; you can resume later |
| **Esc Dismiss** | Close the WCB without affecting the running leader |

---

## Repair loop

If the leader's `workflow.toml` is missing, fails to parse, or references agents the project does not have, awman does not abort immediately. Instead, it launches a **repair agent** — the same leader container and model — with a prompt that includes the exact validation error and the workflow format documentation.

The repair loop runs up to **3 times** before giving up:

```
Attempt 1: leader writes workflow.toml → validate → error found
Attempt 1 repair: repair agent fixes the file → re-validate → passes → proceed
```

```
Attempt 1: leader writes nothing → validate → missing-file error
Attempt 1 repair: repair agent writes a file → validate → bad agent name
Attempt 2 repair: repair agent fixes agent name → validate → passes → proceed
```

If all 3 repair attempts fail, awman surfaces the final error and the path to the last generated file so you can inspect it manually:

```
leader agent failed to produce a valid workflow.toml after 3 repair attempts
last error: workflow.toml references agents with no Dockerfile in the project:
  - "gemini" (expected .awman/Dockerfile.gemini)
  Available agents: claude, maki
file is at: /home/user/.awman/context/workflow/abc123/workflow.toml
```

Each repair agent runs through the same stuck detection → yolo countdown → auto-advance pipeline as the original leader.

---

## Flag rules

| Rule | Error |
|------|-------|
| `--dynamic` with a positional workflow path | `cannot specify a workflow file path with --dynamic; the path is created automatically` |
| `--dynamic` without `--work-item` | `--dynamic requires --work-item` |
| `--leader` without `--dynamic` | `--leader is only valid with --dynamic` |
| `--dynamic --plan` | `--dynamic cannot be used with --plan because dynamic mode enforces --yolo` |
| Malformed `--leader` value | `invalid --leader value: expected agent::model (e.g. claude::claude-opus-4-8)` |

All flag errors are surfaced before any container work begins.

---

## Flags

| Flag | Description |
|------|-------------|
| `--dynamic` | Enable dynamic mode — the leader agent designs the workflow file |
| `--work-item <N>` | Required with `--dynamic`; the work item number to pass to the leader |
| `--leader <agent::model>` | Override the agent container and model used for the leader step |
| `--model <NAME>` | Default model for the generated workflow's steps; also used as the leader model if `--leader` is not set |

Dynamic mode always enforces `--yolo`, `--worktree`, and `--overlay context(workflow)`. Passing any of these explicitly has no effect (they are already active).

---

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| Work item file not found or unreadable | Hard error before any container work; the file is required for the leader prompt |
| Leader writes nothing (no `workflow.toml`) | Repair loop: missing-file error passed to repair agent |
| Leader writes invalid TOML | Repair loop: parse error passed to repair agent |
| Leader references an unknown agent | Repair loop: agent validation error lists unknown agents and available ones |
| Repair agent exhausts 3 attempts | Final error surfaced with the file path for manual inspection |
| Repair agent deletes the file instead of fixing it | Treated as missing-file error on next validation pass; repair loop continues |
| Leader references a valid agent with no built image | Image is built automatically before the workflow starts (not a repair-loop error) |
| Agent image build fails | Hard error with build output; workflow does not start; repair loop is not entered |
| Leader modifies source files outside the context directory | Dynamic run aborts before executing the generated workflow; changed paths are reported |
| User restarts the leader via WCB | Current leader container killed, `workflow.toml` deleted, fresh leader launched |
| User aborts during the leader yolo countdown | Leader container killed, entire dynamic run aborts — no workflow executes |
| User pauses during the leader step | Leader container killed; resume semantics follow the standard workflow pause/resume path |
| Leader becomes unstuck during yolo countdown | Countdown cancelled; leader continues running normally |
| `--leader` and `--model` both set | `--leader` controls the leader's agent and model; `--model` applies to the generated workflow's steps |
| Context directory already contains a `workflow.toml` from a previous run | Deleted before the leader launches; stale files are never executed |

---

[← Runtimes](12-runtimes.md) · [← Back to contents](contents.md)
