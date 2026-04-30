# Work Item: Feature

Title: Yolo Mode
Issue: issuelink

## Summary

Add a `--yolo` flag to the `chat` and `implement` commands that enables fully autonomous agent operation: skipping all permission prompts, applying configured disallowed-tool restrictions, auto-advancing stuck workflow steps via a countdown dialog, and implying `--worktree` when combined with `--workflow`. Also add a per-step snooze option (`d`) to the workflow control dialog for non-yolo users.

## User Stories

### User Story 1:
As a: user

I want to: run `amux implement <work-item> --yolo --workflow steps.md` and have the agent execute the entire workflow without interrupting me for permission checks or stuck-step confirmations

So I can: walk away and return to a finished implementation, trusting that the system will self-recover from stuck states automatically

### User Story 2:
As a: user

I want to: configure `yoloDisallowedTools` in my repo or global config to restrict which tools the agent is allowed to use when `--yolo` is active

So I can: grant broad autonomy while still preventing specific dangerous operations (e.g., `Bash`, `computer`) on a per-repo or global basis

### User Story 3:
As a: user

I want to: press `d` in the workflow control dialog to snooze auto-workflow-controls for the current step without dismissing the dialog's availability entirely

So I can: let a long-running step proceed uninterrupted without having yolo mode on, while still being able to invoke the control dialog manually if needed


## Implementation Details

### 1. `--yolo` Flag on CLI

- Add `yolo: bool` field to both `ChatArgs` and `ImplementArgs` in `src/cli.rs`.
- Flag: `--yolo` (long only, no short alias).
- Help text: `"Enable fully autonomous mode: skip all agent permission prompts, apply yoloDisallowedTools config, and (with --workflow) auto-advance stuck steps after countdown."`.

### 2. Agent Entrypoint — Skip-Permissions Flag

In `src/commands/chat.rs` and `src/commands/implement.rs`, extend `append_yolo_flags()` (new helper, mirrors `append_plan_flags()`):

| Agent | Yolo Flag |
|---|---|
| `claude` | `--dangerously-skip-permissions` |
| `codex` | `--full-auto` |
| `opencode` | *(no equivalent — log a warning, proceed without flag)* |

Call `append_yolo_flags()` when `args.yolo` is true, appending to the entrypoint vector before container launch.

### 3. `yoloDisallowedTools` Config Field

Add to both `RepoConfig` and `GlobalConfig` in `src/config/mod.rs`:

```rust
pub yolo_disallowed_tools: Option<Vec<String>>,
```

JSON key: `"yoloDisallowedTools"` (camelCase to match existing config style).

Resolution priority (highest first):
1. Repo config `yoloDisallowedTools`
2. Global config `yoloDisallowedTools`
3. Empty list (no restriction)

When `--yolo` is active and the merged list is non-empty, append to the entrypoint:

| Agent | Flag |
|---|---|
| `claude` | `--disallowedTools <tool1>,<tool2>,...` |
| `codex` | *(no equivalent — skip with printed warning)* |
| `opencode` | *(no equivalent — skip with printed warning)* |

### 4. `--yolo` + `--workflow` Implies `--worktree`

In `src/commands/implement.rs`, at the start of the implement command handler, if both `args.yolo` and `args.workflow.is_some()` are true, set `args.worktree = true` unconditionally.

Print an informational message to stdout before execution begins:
```
--yolo with --workflow implies --worktree. Running in isolated worktree.
```

This implication should be documented in the CLI help text for `--yolo` and in `docs/`.

### 5. Yolo Stuck-Detection: Countdown Dialog

When `--yolo` is active and a workflow tab becomes stuck (10 s inactivity, same `is_stuck()` criteria), skip the `WorkflowControlBoard` dialog and instead open a new `WorkflowYoloCountdown` dialog.

**New dialog state** (`src/tui/state.rs`):

```rust
WorkflowYoloCountdown {
    current_step: String,
    started_at: Instant,   // when the countdown began
    duration: Duration,    // configurable, default 60 s
}
```

**Rendering** (`src/tui/render.rs`):
- Display step name, a live countdown (seconds remaining), and a brief message: `"No activity detected. Advancing to next step in <N>s..."`.
- Update every tick via the existing event loop.

**Timeout action** (`src/tui/mod.rs` / `src/tui/state.rs`):
- When `started_at.elapsed() >= duration`, automatically trigger `WorkflowNextInNewContainer`.
- If the current step is the last step, trigger `WorkflowFinish` instead.

**Cancellation**:
- Any PTY output received from the container during the countdown cancels the countdown and closes the dialog (agent is no longer stuck).
- User can press `Esc` to dismiss; this applies the normal 60 s `STUCK_DIALOG_BACKOFF` so the countdown won't immediately re-open.

**Countdown duration constant**: `YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60)` in `src/tui/state.rs`.

### 6. Non-Yolo: `d` Key — Disable Auto-Workflow Controls for Current Step

In the `WorkflowControlBoard` dialog:

- Add a new key binding `d` → `DisableAutoWorkflowForStep`.
- When pressed, set a flag `auto_workflow_disabled_for_step: bool` on the current workflow tab state.
- While this flag is set, the auto-open logic in the event loop skips stuck-dialog activation for this tab.
- Reset the flag when the workflow advances to the next step (i.e., on any `WorkflowNext*` or `WorkflowFinish` action, or when the tab's current step changes).
- Update the dialog footer hint line to include: `[d]isable controls auto-popup for this step`.
- The manual `Ctrl+W` shortcut when the container window is minimized must still open the dialog even when the flag is set.


## Edge Case Considerations

- **`--yolo` without `--workflow`**: `--worktree` is NOT implied. The flag only affects agent permission flags and disallowed tools. Document this clearly.
- **Last workflow step + countdown expires**: trigger `WorkflowFinish`, not `WorkflowNextInNewContainer`. Guard with the same last-step check used in the control board.
- **Container exits before countdown completes**: the countdown dialog should close; normal post-run flow applies (merge prompt, etc.).
- **`opencode` + `--yolo`**: log a warning that no skip-permissions flag is available for opencode; continue without it rather than failing.
- **`yoloDisallowedTools` in both configs**: repo config wins entirely (not merged). Document this precedence.
- **Countdown reset on PTY output**: ensure any byte of PTY output dismisses the countdown, not only "meaningful" output, to avoid races with slow-starting agents.
- **`d` flag reset on step change**: ensure `auto_workflow_disabled_for_step` resets before the new step's stuck-detection logic runs, so the new step's auto-controls are active immediately.
- **`--yolo` + `--worktree` explicitly passed together**: no conflict; no duplicate worktree creation. The implication is idempotent.
- **Worktree implication printed only once**: if the user explicitly passed `--worktree`, do not print the implication message.


## Test Considerations

- **Unit tests** (`src/commands/chat.rs`, `src/commands/implement.rs`):
  - `append_yolo_flags()` returns correct flags per agent (`claude`, `codex`, `opencode`).
  - `yoloDisallowedTools` resolution: repo config wins over global; empty list produces no `--disallowedTools` argument.
  - Worktree implication: `yolo=true, workflow=Some(_)` sets `worktree=true`; `yolo=true, workflow=None` does not.

- **Unit tests** (`src/config/mod.rs`):
  - `yolo_disallowed_tools` deserializes correctly from JSON for both `RepoConfig` and `GlobalConfig`.
  - Priority resolution returns repo value when both are set.

- **Unit tests** (`src/tui/state.rs`):
  - `is_stuck()` still returns correct values (no regression).
  - `WorkflowYoloCountdown` transitions to `WorkflowNextInNewContainer` after `YOLO_COUNTDOWN_DURATION`.
  - `WorkflowYoloCountdown` closes on PTY output received before timeout.
  - `auto_workflow_disabled_for_step` prevents auto-open; resets on step change.

- **Integration / E2E tests**:
  - `amux chat --yolo` passes `--dangerously-skip-permissions` in the Docker entrypoint (inspect built args).
  - `amux implement <N> --yolo --workflow <file>` sets `worktree=true` and prints the implication message.
  - `amux implement <N> --yolo --worktree --workflow <file>` does NOT print the implication message.
  - Workflow countdown dialog auto-advances after timeout in a simulated stuck scenario.


## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Mirror `append_plan_flags()` in `src/commands/chat.rs` and `src/commands/implement.rs` for the new `append_yolo_flags()` helper.
- Mirror `WorkflowControlBoard` state/render/input patterns for `WorkflowYoloCountdown`.
- Add `yolo_disallowed_tools` to `RepoConfig` and `GlobalConfig` in `src/config/mod.rs` using the same `Option<Vec<String>>` / `#[serde(rename = "camelCase")]` pattern used for existing fields.
- The `auto_workflow_disabled_for_step` flag belongs on the per-tab workflow state alongside `workflow_stuck_dialog_opened` and `workflow_stuck_dialog_dismissed_at`.
- Update `docs/` to document `--yolo`, the worktree implication, and `yoloDisallowedTools` config. Do not create separate per-work-item documentation files.
