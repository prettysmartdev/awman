# Yolo Mode

Yolo mode is amux's fully autonomous operation mode. When `--yolo` is active, the agent skips all permission prompts and proceeds without pausing for confirmation on any action it would normally stop for.

Use it when you want to hand a task to the agent and return to a finished result — no babysitting required.

---

## When to use yolo mode

Yolo mode is appropriate when:

- You have a well-specified work item and trust the agent to implement it correctly
- You're running a multi-step workflow and want it to complete end-to-end with no manual advancement
- You've already reviewed the plan in a `--plan` session and are confident in the approach
- The task is running in an isolated worktree (implied automatically when `--yolo --workflow` are combined), so even if the output isn't ideal it's easy to discard

Yolo mode is **not** appropriate for:

- Tasks where the agent will encounter decisions that genuinely require your input
- Open-ended `chat` sessions where you want ongoing interaction
- Any situation where agent mistakes would be difficult to undo (tip: use `--worktree` to contain the blast radius)

---

## Basic usage

```sh
amux exec workflow aspec/workflows/implement-feature.md --yolo
amux chat --yolo
```

For the safest yolo experience — fully autonomous, changes isolated to a branch, easy to review or discard:

```sh
amux exec workflow aspec/workflows/implement-feature.md --yolo
```

This implies `--worktree` automatically (see below).

---

## What `--yolo` does

### 1. Skips all agent permission prompts

The agent-specific skip-permissions flag is appended to the container entrypoint before launch:

| Agent | Flag appended |
|-------|--------------|
| `claude` | `--dangerously-skip-permissions` |
| `codex` | `--full-auto` |
| `opencode` | *(no equivalent — a warning is printed, flag omitted)* |
| `maki` | `--yolo` |
| `gemini` | `--yolo` |
| `copilot` | `--autopilot` (copilot's only CLI autonomous mode) |
| `crush` | `--yolo` (inserted before the `run` subcommand: `crush --yolo run`) |
| `cline` | `--yolo` (on the `task` subcommand) |

### 2. Applies `yoloDisallowedTools`

Any tools listed in `yoloDisallowedTools` in your config are passed to the agent as a deny list. This lets you grant broad autonomy while still preventing specific dangerous operations. See [Disallowed tools](#disallowed-tools) below.

| Agent | Flag used |
|-------|-----------|
| `claude` | `--disallowedTools tool1,tool2,...` |
| `codex` | *(no equivalent — a warning is printed)* |
| `opencode` | *(no equivalent — a warning is printed)* |
| `maki` | *(no equivalent — a warning is printed)* |
| `gemini` | *(no equivalent — a warning is printed)* |
| `copilot` | *(no equivalent — a warning is printed)* |
| `crush` | *(no equivalent — a warning is printed)* |
| `cline` | *(no equivalent — a warning is printed)* |

### 3. Implies `--worktree` for workflow execution

When running a workflow with `--yolo`, amux automatically creates an isolated Git worktree. A message is printed at startup:

```
--yolo with workflow execution implies --worktree. Running in isolated worktree.
```

If `--worktree` is also passed explicitly, it is silently accepted — no duplicate worktree is created.

When `--yolo` is used with other commands (e.g. `chat`), `--worktree` is **not** implied. The flag only affects permission prompts and disallowed tools. Use `--worktree` explicitly if you want isolation.

### 4. Auto-advances stuck workflow steps

When a workflow step goes silent for 30 seconds, amux begins a **yolo countdown** instead of opening the manual [workflow control board](04-workflows.md#workflow-control-board). The countdown timer automatically advances to the next step when it expires. How the countdown is presented depends on whether the tab is active or in the background.

**Active tab — yolo countdown dialog:**

When the stuck tab is currently active, the countdown dialog opens:

```
╭─────── Yolo: Auto-Advance ──────────────╮
│ Step: implement                          │
│                                          │
│  No activity detected.                   │
│  Advancing to next step in  47s...       │
│                                          │
│                    [Esc] cancel          │
╰──────────────────────────────────────────╯
```

**Active-tab suppression:** If you are actively pressing keys or scrolling on the tab, the stuck timer is held back and the dialog will not open. Both the container and the user must be idle for 30 seconds before the countdown starts.

**Background tab — tab bar countdown:**

When the stuck tab is in the background, no dialog opens. Instead, the tab bar shows a live countdown directly: the tab alternates between yellow and purple every second, with the label cycling between `⚠️ yolo in N` and `🤘 yolo in N` (where `N` is the remaining seconds):

```
┌─ Tab 1: myproject ─────────┬─ Tab 2 ⚠️  yolo in 38 ─────┐
│  chat                        │                              │
└──────────────────────────────┴──────────────────────────────┘
```

This lets you monitor all tabs' countdown state without leaving your current work.

**Switching to a background countdown tab:**

If you switch to a tab that has a countdown in progress, the yolo dialog opens immediately, showing the time remaining — the countdown is not restarted from 60 seconds. You can then let it expire, press **Esc** to dismiss, or navigate away.

**Switching away from an active yolo dialog:**

Press **Ctrl+A** or **Ctrl+D** while the yolo dialog is open to navigate to the previous or next tab. The dialog closes and the countdown continues in the background (shown in the tab bar). You are not forced to resolve the dialog before switching away.

The countdown runs for **60 seconds**. When it expires:
- If this is not the last step — amux advances to the next step in a new container
- If this is the last step — the workflow transitions to complete

**Cancellation:**
- Any PTY output during the countdown immediately dismisses the countdown — the agent is no longer stuck
- Press **Esc** to dismiss the active-tab dialog manually; if the container goes silent again, a fresh 60-second countdown begins (there is no backoff between cancellation and the next countdown)

---

## Background yolo countdown

When you are working across multiple tabs and a background tab's yolo workflow step goes silent, amux does not interrupt you with a dialog. Instead, the tab bar shows a live countdown for each affected tab:

- The tab alternates between **yellow** and **purple** every second
- The label cycles between `⚠️  yolo in N` and `🤘 yolo in N` (where `N` is the seconds remaining)
- Multiple background tabs each have independent countdowns; they alternate colors at their own pace

```
┌─ Tab 1: myproject ─────────┬─ Tab 2 🤘 yolo in 23 ──────┐
│  chat                        │                              │
└──────────────────────────────┴──────────────────────────────┘
```

**Switching to a countdown tab:** The yolo dialog opens immediately with the remaining time — the timer is not restarted from 60 seconds.

**Switching away from the dialog:** Press **Ctrl+A** or **Ctrl+D** to navigate to another tab while the yolo dialog is open. The dialog closes and the countdown continues in the background. You are not forced to act before switching away.

**When the countdown expires in the background:** The workflow auto-advances without requiring you to switch to the tab. The tab returns to its normal color and label as soon as it moves to the next step.

**When output resumes mid-countdown:** If the container produces new output before the countdown expires, the countdown resets and the tab returns to its normal color. If the container goes silent again, a fresh 60-second countdown begins.

---

## Disallowed tools

Add `yoloDisallowedTools` to your per-repo or global config to restrict which tools the agent may use even under full autonomy:

```json
{
  "yoloDisallowedTools": ["Bash", "computer"]
}
```

This is your safety net for operations you never want the agent to perform autonomously, regardless of how well-specified the task is. Common choices:

- `"Bash"` — prevents arbitrary shell command execution
- `"computer"` — prevents GUI automation

**Config precedence:** per-repo config takes precedence over global config entirely (lists are not merged). To inherit the global list for a specific repo, omit the field from the repo config.

See [Configuration](07-configuration.md) for the full config reference.

---

## `--auto` mode

`--auto` is a less permissive alternative to `--yolo`. The agent auto-approves file edits and writes but still pauses before shell commands and other high-risk operations. Use it when you want to reduce confirmation prompts without granting full autonomy.

| Agent | `--auto` flag |
|-------|--------------|
| `claude` | `--permission-mode auto` |
| `codex` | `--full-auto` |
| `opencode` | *(no equivalent — warning printed)* |
| `maki` | `--yolo` (maki's own flag) |
| `gemini` | `--approval-mode=auto_edit` |
| `copilot` | `--autopilot` (copilot has no intermediate mode; same flag as `--yolo`) |
| `crush` | `--yolo` (crush has no intermediate mode; same flag as `--yolo`; a warning is printed) |
| `cline` | `--auto-approve-all` (auto-approves actions while keeping interactive mode) |

`--auto` applies `yoloDisallowedTools` config identically to `--yolo`. Combined with `--workflow`, it implies `--worktree` but does **not** auto-advance stuck steps (the countdown is `--yolo`-only).

When both `--yolo` and `--auto` are passed, `--yolo` wins.

---

## Security considerations

- `--yolo` removes the human checkpoints that catch unintended agent actions. Only use it with agents and work items you trust.
- The `yoloDisallowedTools` config provides a floor — operations the agent can never perform autonomously, even with `--yolo`.
- Combine `--yolo` with `--workflow` to get automatic `--worktree` isolation, making it easy to review the full diff before merging into your main branch.
- `--yolo --workflow` is the recommended pattern for long-running autonomous tasks: isolated branch, structured phases, auto-advancing, easy to discard if the output isn't right.
- Gemini's `--yolo` flag skips all tool confirmations including shell commands. Gemini's `--approval-mode=auto_edit` (amux `--auto`) is the more conservative choice — file writes are approved automatically but shell operations are not.
- Copilot maps both `--yolo` and `--auto` to `--autopilot` — there is no intermediate CLI mode. Use `yoloDisallowedTools` config to restrict specific operations if needed (though copilot does not support the flag directly; a warning is printed and the session launches unrestricted).
- Crush maps both `--yolo` and `--auto` to its `--yolo` flag, which auto-approves all permissions. A warning is printed when `--auto` is used, since crush has no intermediate mode.
- Cline's `--auto-approve-all` (amux `--auto`) keeps interactive mode while auto-approving actions. Cline's `--yolo` (amux `--yolo`) fully skips confirmations and implies non-interactive operation.

---

## Examples

```sh
# Run a workflow with no prompts, changes in an isolated worktree
amux exec workflow aspec/workflows/implement-feature.md --yolo

# Run a workflow with explicit worktree flag — identical to omitting it
amux exec workflow aspec/workflows/implement-feature.md --yolo --worktree

# Autonomous chat session with Bash tool blocked
# (add to aspec/.amux.json: "yoloDisallowedTools": ["Bash"])
amux chat --yolo
```

---

[← Workflows](04-workflows.md) · [Next: Configuration →](07-configuration.md)
