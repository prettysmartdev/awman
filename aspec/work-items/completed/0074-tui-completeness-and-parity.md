# Work Item: Feature

Title: TUI completeness and feature parity
Issue: n/a — consolidation of all remaining parity gaps between old-amux and new-amux

## Prerequisites

- Familiarity with the current TUI codebase under `src/frontend/tui/` and the engine under `src/engine/`.
- The old-amux reference implementation under `oldsrc/` (read-only reference — do not modify).

The implementing agent MUST read:

- `src/engine/workflow/mod.rs` (the engine driver loop).
- `src/engine/workflow/frontend.rs` (the `WorkflowFrontend` trait).
- `src/engine/workflow/actions.rs` (decision types and enums).
- `src/frontend/tui/mod.rs` (key handling, event loop).
- `src/frontend/tui/render.rs` (all rendering).
- `src/frontend/tui/tabs.rs` (per-tab state).
- `src/frontend/tui/keymap.rs` (keybinding definitions).
- `src/frontend/tui/per_command/workflow_frontend.rs` (TUI workflow implementation).
- `src/frontend/tui/per_command/worktree_lifecycle.rs` (worktree dialogs).
- `src/frontend/tui/container_view.rs` (container overlay rendering).
- `src/frontend/tui/workflow_view.rs` (workflow strip rendering).
- `src/engine/init/mod.rs` (init engine phases).

When uncertain about architecture trade-offs, ASK THE DEVELOPER rather than picking a half-baked path.

## Context

The new four-layer architecture is in place and the TUI frontend is functional for all major workflows. However, a final audit comparing old-amux and new-amux reveals approximately a dozen gaps ranging from missing engine integration (mid-step interrupt, auto-disable toggle) to visual polish (command box hints, stuck-step indicators) to command-level parity (init flow differences). Each gap is documented as an independent sub-item below.

**Design principle:** every sub-item (A through L) is self-contained and can be implemented by an independent sub-agent in parallel. Sub-items have no ordering dependencies unless explicitly noted.

## Summary

- **A.** Add an engine control-board channel so the user can open the Workflow Control Board mid-step without disrupting the running container. The step continues executing while the user decides; only destructive actions (restart, advance, rewind, abort) kill the container. Continue-in-current-container queues a message to the running agent.
- **B.** Wire the `auto_disabled` step set from the TUI into the engine so `[d]`-toggled steps actually skip auto-advance.
- **C.** Add stuck-step visual indicators to the workflow strip (not just the tab label).
- **D.** Send a PTY resize event when cycling the container window state, not just on terminal resize.
- **E.** Command-box polish: horizontal scroll for long input, `q to quit` ghost text.
- **F.** Suggestions row polish: middle-ellipsis path truncation, em-dash flag hints from the command catalogue.
- **G.** ConfigShow read-only field feedback: show a toast when Enter is pressed on a read-only row.
- **H.** Lightweight `WorkflowStepConfirm` dialog as an alternative to the full Workflow Control Board.
- **I.** Mouse-wheel scroll inside the workflow strip to view overflowed parallel groups.
- **J.** Init command parity: conditional work-items setup, detailed interactive prompt text, explicit directory creation, non-TTY consistency.
- **K.** Worktree cancel-cleanup: keep the worktree on disk when the user aborts a workflow so they can resume later.
- **L.** Documentation refresh: update `docs/` to cover all new and changed TUI behaviour.

## User Stories

### User Story 1:
As a: amux user running a long, multi-step workflow

I want to: press Ctrl+W mid-step to open the Workflow Control Board without disrupting the running step, then choose to continue, queue a message, restart, advance, or just dismiss and let it keep running

So I can: make workflow decisions at any time without being forced to wait or abort.

### User Story 2:
As a: amux user who toggled `[d]` on a workflow step to disable auto-advance

I want to: have the engine actually pause at that step instead of auto-advancing anyway

So I can: review agent output before deciding what to do next.

### User Story 3:
As a: amux user watching a multi-step workflow

I want to: see at a glance which steps are stuck (in the workflow strip, not just the tab label)

So I can: intervene before a stalled step wastes time.

### User Story 4:
As a: amux user typing a long command

I want to: see the full input via horizontal scroll and know I can press `q` to quit when idle

So I can: use the command box confidently without guessing at clipped text.

### User Story 5:
As a: new amux user running `amux init` for the first time

I want to: get the same guided experience as old-amux, with helpful explanations at each prompt

So I can: set up my repo correctly without consulting external docs.

### User Story 6:
As a: amux user who aborted a workflow running in a worktree

I want to: have the worktree preserved on disk so I can resume later

So I can: avoid losing partial progress from completed steps.

---

## Implementation Details

Each sub-item below is designed to be implemented independently and in parallel. File paths, structs, and methods reference the codebase as of the writing of this work item.

---

### A. Mid-step workflow control board (non-destructive open)

**Problem:** The engine's `step_once()` blocks on `execution.wait()`. The user can open the Workflow Control Board between steps or abort via Ctrl+C, but there is no way to open the WCB mid-step to make a decision about the running step. The user must wait for the step to finish naturally or abort the entire workflow.

**Key design principle:** Pressing Ctrl+W mid-step opens the WCB dialog immediately **without killing the running container**. The current step continues executing in the background while the user considers their options. Only when the user selects an action that requires it (Restart, Advance, CancelToPrevious, Abort) does the engine kill the container. `ContinueInCurrentContainer` remains available because messages can be queued to a running agent.

**Engine changes (`src/engine/workflow/mod.rs`):**

Add a control-board request channel to `WorkflowEngine`:

```rust
pub struct WorkflowEngine {
    // ...existing fields
    control_board_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ControlBoardRequest>>,
}

#[derive(Debug, Clone)]
pub enum ControlBoardRequest {
    /// User pressed Ctrl+W. Engine should compute AvailableActions and call
    /// user_choose_next_action while the step continues running.
    OpenControlBoard,
}
```

Extend the `WorkflowFrontend` trait (`src/engine/workflow/frontend.rs`) with a default-no-op method:

```rust
fn set_control_board_sender(
    &mut self,
    _tx: tokio::sync::mpsc::UnboundedSender<ControlBoardRequest>,
) {}
```

Modify the engine's run loop to `tokio::select!` between the step future and the control-board request:

```rust
loop {
    let mut step_fut = Box::pin(self.step_once());
    tokio::select! {
        outcome = &mut step_fut => { /* normal completion path */ }
        Some(req) = recv_request(self.control_board_rx.as_mut()) => match req {
            ControlBoardRequest::OpenControlBoard => {
                // 1. DO NOT cancel the running container. It keeps running.
                // 2. Compute AvailableActions for the mid-step case (see below).
                //    The step is still Running — all actions are available.
                // 3. Call frontend.user_choose_next_action(state, available).
                // 4. Apply the user's choice:
                //    - ContinueInCurrentContainer: queue the prompt, resume waiting.
                //    - Pause/Dismiss: resume waiting on step_fut (no-op, step keeps going).
                //    - Restart/Advance/CancelToPrevious/Abort/FinishWorkflow:
                //      NOW cancel the container, then apply the action.
                // 5. Continue the outer loop.
            }
        }
    }
}
```

The critical insight: the `tokio::select!` suspends (but does not cancel) `step_fut` while the user is in the dialog. If the step completes naturally while the dialog is open, that completion is picked up on the next iteration. The engine can handle this by checking execution status before applying a destructive action.

**Mid-step `AvailableActions` rules:**

| Action | Availability | Effect on running container |
|---|---|---|
| `ContinueInCurrentContainer` | Available IFF there is another step to launch — messages can be queued to the running agent. | None — container keeps running, prompt is injected. |
| `LaunchNext` | Available IFF there is another step to launch (treats current as force-succeeded, advances) | Kills container, then launches next step. |
| `RestartCurrentStep` | Always — kills and re-runs from Pending. | Kills container, re-launches same step. |
| `CancelToPreviousStep` | Available iff a prior step exists. | Kills container, rewinds. |
| `FinishWorkflow` | Available iff this is the last step. | Kills container, marks as finished. |
| `Pause` | Always — engine returns, step is killed. | Kills container, returns Paused outcome. |
| `Abort` | Always. | Kills container, returns Aborted outcome. |
| `Dismiss` (Esc) | Always — user changed their mind. | **None** — dialog closes, step continues running undisturbed. |

**Action application logic:**

```rust
match user_choice {
    NextAction::ContinueInCurrentContainer { prompt } => {
        // Queue the prompt to the running container's stdin.
        // Do NOT cancel. Resume waiting on step_fut.
        if let Some(exec) = self.current_execution.as_ref() {
            exec.inject_prompt(&prompt)?;
        }
        // Continue the loop — step_fut resumes on next select! iteration.
    }
    NextAction::Pause | NextAction::Dismiss => {
        // For Pause: kill container, persist state, return Paused.
        // For Dismiss: no-op, resume waiting on step_fut.
    }
    destructive_action => {
        // Restart, Advance, CancelToPrevious, FinishWorkflow, Abort:
        // NOW kill the running container.
        if let Some(exec) = self.current_execution.as_ref() {
            let _ = exec.cancel();
        }
        // Apply the action (same logic as between-steps actions).
    }
}
```

**Persistence (`src/engine/workflow/state.rs` or equivalent):**

- Step state remains `Running` while the dialog is open (no premature state change).
- On `Restart`: cancel container, set step to `Pending`, re-run.
- On `Advance`: cancel container, set step to `Succeeded` with `StepCompletionMode::UserForcedAdvance` marker (vs `ContainerExited`). Resume must not re-run force-completed steps.
- On `CancelToPrevious`: cancel container, flip both current and previous to `Pending`.
- On `FinishWorkflow`: cancel container, remaining steps go to `Skipped`.
- On `ContinueInCurrentContainer`: mark current step `Succeeded`, advance to next step — next step is queued in the current contaienr.
- On `Dismiss`: no state change — step is still Running.
- Schema version bumps; add migration defaulting old steps to `ContainerExited`.

**TUI wiring (`src/frontend/tui/`):**

Per-tab additions on `Tab` (`tabs.rs`):
- `control_board_tx: Option<UnboundedSender<ControlBoardRequest>>` — set when a workflow spawns.
- `last_available_actions: Arc<Mutex<Option<AvailableActions>>>` — cached by `user_choose_next_action` so Ctrl+W between engine prompts can re-open the WCB without round-tripping.

`Ctrl+W` handler (`mod.rs`):
1. If no workflow active → push status-bar hint "no workflow running" and no-op.
2. If engine is between steps (cached actions exist, no step Running) → open WCB locally with cached actions.
3. If step is Running → send `ControlBoardRequest::OpenControlBoard` over `tab.control_board_tx`. The engine computes actions and calls `user_choose_next_action` which opens the WCB through the normal dialog channel. The container continues running — the user can see its output updating behind the dialog.

Extend `WorkflowControlBoardState` (`dialogs/mod.rs`) with `is_mid_step: bool`. When true:
- Title: `Workflow Control (step running)`.
- `Restart`, `Advance`, `CancelToPrevious`, and `Abort` lines get a sub-bullet `↳ kills running container` in DarkGray.
- `Continue in current container` line gets a sub-bullet `↳ queues message to running agent` in DarkGray.
- `Esc` hint reads `dismiss (step keeps running)`.

**CLI/headless:** `set_control_board_sender` default is a no-op — no changes needed.

**Edge cases:**
- **Step completes naturally while WCB is open:** The engine detects this when it goes to apply the user's choice. If the container is already gone: treat `Restart` as a fresh re-run from Pending (container already exited, just restart); treat `Advance` as a no-op (step already succeeded naturally); treat `ContinueInCurrentContainer` as stale (inform user step already completed, re-open WCB with between-steps actions); treat `Dismiss` as normal between-steps flow.
- **Ctrl+W when WCB is already open** → no-op (Ctrl+W in `FocusContext::Dialog` is unbound).
- **Double request before engine responds** → second send is harmless (unbounded channel queues it; engine drains one and ignores duplicates).
- **`ContinueInCurrentContainer` prompt injection** → uses the existing `inject_prompt` path on the live container's stdin channel. The container is still running so the channel is active.
- **Crash while WCB is open mid-step** → on resume, `interrupted_running_steps()` sees the step as `Running` with a dead container_id. Existing resume logic handles this — user is prompted to Restart vs Skip.
- **`[d] disable auto-advance` toggle** → mid-step open is always user-initiated; the auto-advance gate doesn't apply.
- **Pause mid-step** → this is the only "soft destructive" action. The container must be killed because the engine is returning control. On resume, the step will re-run (same as old-amux "resume re-runs the last incomplete step").

---

### B. Engine consumption of `auto_disabled` set

**Problem:** The TUI's `[d]` toggle in the WCB adds/removes step names from `tab.workflow_state.auto_disabled` (a `HashSet<String>` on `WorkflowViewState` in `tabs.rs`). A lock icon renders in the workflow strip (`workflow_view.rs`). But the engine never consults this set — it auto-advances regardless, making the toggle cosmetic-only. The TUI does use it to suppress the stuck-detection auto-dialog (`app.rs` ~line 421), but that's a workaround, not the correct integration.

**Engine trait extension (`src/engine/workflow/frontend.rs`):**

Add a new method with a default that preserves backwards compatibility:

```rust
fn should_auto_advance(&self, step_name: &str) -> bool {
    true // CLI and headless always auto-advance
}
```

**Engine integration (`src/engine/workflow/mod.rs`):**

Before entering the yolo countdown loop for a step, call `self.frontend.should_auto_advance(step_name)`. If `false`, skip the countdown entirely and fall through to `user_choose_next_action` — same path as non-yolo mode.

**TUI implementation (`src/frontend/tui/per_command/workflow_frontend.rs`):**

Implement `should_auto_advance` by reading the shared `auto_disabled` set:

```rust
fn should_auto_advance(&self, step_name: &str) -> bool {
    let ws = self.workflow_state.lock().unwrap();
    !ws.auto_disabled.contains(step_name)
}
```

**Remove workaround:** The stuck-detection guard in `app.rs` that checks `auto_disabled` to suppress the dialog can be simplified now — the engine itself won't auto-advance, so the TUI doesn't need to second-guess it.

**Edge cases:**
- User toggles `[d]` while the yolo countdown is already running for that step → the countdown is already in progress on the engine thread. The TUI's yolo Esc/Ctrl+W path still works as an escape valve. Document that [d] takes effect on the *next* time the engine evaluates that step, not retroactively.
- Step names with special characters → `HashSet<String>` comparison is exact-match. No normalization needed.

---

### C. Stuck-detection visual integration in workflow strip

**Problem:** When a step is stuck, the tab label turns yellow with a `⚠️` prefix (`tabs.rs`), and `report_step_stuck`/`report_step_unstuck` emit status-log lines (`workflow_frontend.rs`). But the workflow strip itself (`workflow_view.rs`) shows no visual indicator — the step box looks identical whether stuck or healthy.

**Data plumbing:**

Add `stuck: bool` to `WorkflowStepView` in `tabs.rs`. Default `false`.

In `report_step_stuck(step_name)` (`workflow_frontend.rs`): set `view.steps[i].stuck = true` for the matching step in the shared `workflow_state`.

In `report_step_unstuck(step_name)`: clear it.

**Strip rendering (`workflow_view.rs`):**

When rendering a step box where `step.stuck == true`:
- Prefix the step name label with `⚠️ ` (yellow).
- Change the box border from the normal status color to Yellow.
- This is a small visual addition — it should be clearly visible but not obscure the step name.

**Edge cases:**
- Step unsticks before the next render frame → `stuck` is already cleared, no stale indicator.
- Multiple steps stuck simultaneously (parallel group) → each step's box independently shows the indicator.

---

### D. PTY resize on container window cycle

**Problem:** `handle_resize` (`mod.rs` ~line 693) correctly forwards the terminal size to the container PTY via `container_resize_tx` — but this only fires on terminal resize events. Cycling the container window via Ctrl+M (`CycleContainerWindow` action, `mod.rs` ~line 264) changes the vt100 grid dimensions without sending a resize. The PTY and the process inside the container think the terminal is still the old size.

**Fix (`src/frontend/tui/mod.rs`):**

After `tab.container_window_state.cycle()` in the `CycleContainerWindow` handler, if the new state is not `Hidden`, compute the new inner area and send a resize:

```rust
Action::CycleContainerWindow => {
    let tab = app.active_tab_mut();
    tab.container_window_state.cycle();
    if tab.container_window_state != ContainerWindowState::Hidden {
        if let Some(ref tx) = tab.container_resize_tx {
            let (cols, rows) = compute_container_inner_size(terminal_size);
            let _ = tx.send((cols, rows));
        }
    }
}
```

Where `compute_container_inner_size` is the existing function that computes 95% width, full height minus borders.

**Edge cases:**
- `container_resize_tx` is `None` (no container running) → the `if let` guard handles it.
- Cycling to `Hidden` → no resize needed since the PTY isn't visible. Cycling back to `Maximized` later will send the resize.

---

### E. Command-box visual polish

**Problem:** Two gaps vs old-amux in the command box area.

#### E.1 Horizontal scroll for long input

**Current state (`render.rs` ~line 454):** The command box renders the full input text at a fixed position. When the text is wider than the box, the cursor goes off-screen and the tail is clipped.

**Fix (`render.rs`):**

Calculate a `scroll_offset` for the visible portion of the input line:

```rust
let visible_width = inner.width as usize;
let cursor_col = /* current cursor column */;
let scroll_offset = if cursor_col >= visible_width {
    cursor_col - visible_width + 1
} else {
    0
};
let visible_text = &input_text[scroll_offset..];
```

Render `visible_text` instead of the full input. Adjust the cursor position by subtracting `scroll_offset`.

For multi-line input (lines joined with `↵`), apply scrolling to the flattened single-line representation since the command box is a single visible row.

#### E.2 `q to quit` ghost text

**Current state:** When the command box is empty and focused, it shows a blinking cursor with no hint text. Old amux wired `q` on an empty command box to open `QuitConfirm` (this already works in new-amux), but the hint was never shown.

**Fix (`render.rs`):**

When the command box is empty, focused, and the tab is `Idle` or `Done`:

```rust
if input_text.is_empty() && matches!(phase, Idle | Done { .. }) {
    let hint = Span::styled("q to quit", Style::default().fg(Color::DarkGray));
    // render hint at the input position
}
```

The hint disappears as soon as the user types any character (the input is no longer empty).

---

### F. Suggestions row enhancements

**Problem:** Two remaining gaps in the suggestion/context row below the command box.

#### F.1 Long path truncation with middle ellipsis

**Current state (`render.rs` ~line 517):** Worktree and CWD paths render in full. If the path is longer than the available row width, it overflows and is clipped by the terminal.

**Fix:**

Add a `truncate_middle(path: &str, max_width: usize) -> String` helper:

```rust
fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let ellipsis = "…";
    let available = max - ellipsis.len();
    let prefix_len = available / 2;
    let suffix_len = available - prefix_len;
    format!("{}{}{}", &s[..prefix_len], ellipsis, &s[s.len() - suffix_len..])
}
```

Apply it to the path display in `render_suggestion_row`:

```rust
let max_path_width = area.width as usize - label_width - 4; // padding
let display_path = truncate_middle(&path_str, max_path_width);
```

#### F.2 Suggestion flag hints with em-dash

**Current state:** Autocomplete suggestions show only the flag name (e.g. `--yolo`). Old-amux showed `--yolo  —  enable auto-advance mode` with an em-dash separator and a hint string pulled from the command catalogue.

**Fix:**

The `CommandCatalogue` (`src/command/dispatch/catalogue.rs`) already stores flag metadata. When rendering suggestions in `render_suggestion_row`, if the suggestion matches a catalogue flag, append ` — {hint}` in DarkGray after the flag name:

```rust
for suggestion in &suggestions {
    let hint = catalogue.flag_hint(command, &suggestion.flag);
    if let Some(h) = hint {
        // render: suggestion.flag (normal) + " — " + h (DarkGray)
    }
}
```

If `CommandCatalogue` doesn't yet expose `flag_hint()`, add a lookup method that returns the flag's description string. Check `oldsrc/` for the exact hint strings if the catalogue doesn't have them.

---

### G. ConfigShow read-only field feedback

**Problem:** When the user presses Enter on a read-only row in the ConfigShow dialog, nothing happens — no visual cue, no error message (`mod.rs` ~line 909). The user doesn't know why the field won't open for editing.

**Fix (`src/frontend/tui/mod.rs`):**

In the ConfigShow Enter handler, when `row.read_only` is true, push a transient status-bar message:

```rust
if row.read_only {
    app.status_bar.text = "This field is read-only".to_string();
    return;
}
```

The status bar already renders on every frame and clears on the next user action, so this is a natural toast mechanism.

---

### H. Lightweight `WorkflowStepConfirm` dialog

**Problem:** Old-amux had a lightweight "advance to next step?" confirmation distinct from the full Workflow Control Board, used in non-yolo, non-auto mode after each step. New-amux funnels everything through the heavier WCB. This adds cognitive overhead for the simple case where the user just wants to say "yes, continue."

**New dialog type (`src/frontend/tui/dialogs/mod.rs`):**

Add `Dialog::WorkflowStepConfirm { completed_step: String, next_step: String }` and the corresponding `DialogRequest` variant.

**Rendering (`render.rs`):**

A compact dialog: `Step 'build' done. Advance to 'test'? [Enter] yes / [Esc] pause / [Ctrl+W] full control board`

Small centered box, no list navigation — just three keybindings.

**Trigger (`workflow_frontend.rs`):**

In `user_choose_next_action`, when the engine offers a straightforward advance (single next step, no failures, not mid-step), show `WorkflowStepConfirm` instead of the full WCB. The user can escalate to the full WCB via Ctrl+W.

**Key handling (`mod.rs`):**

- `Enter` → respond with `NextAction::LaunchNext`.
- `Esc` → respond with `NextAction::Pause`.
- `Ctrl+W` → dismiss this dialog and open the full WCB with the same `AvailableActions`.

**Edge cases:**
- Multiple next steps (parallel group about to fan out) → fall through to the full WCB.
- Step failure → the existing `WorkflowStepError` dialog handles this; `StepConfirm` is never shown for failures.

---

### I. Mouse-wheel scroll in workflow strip

**Problem:** When a parallel group has more steps than visible rows, the strip shows `+ N more…` with no way to see the hidden steps. Mouse-wheel scrolling over the strip area would let the user cycle through them.

**State (`tabs.rs`):**

Add `workflow_strip_scroll_offset: usize` to `Tab`. Default `0`.

**Mouse handling (`mod.rs`):**

In `handle_mouse_event`, check if the mouse position falls within the workflow strip area (stored in a `last_strip_rect: Option<Rect>` on `Tab`, set during rendering). On `ScrollUp` → decrement offset (clamped to 0). On `ScrollDown` → increment offset (clamped to max overflow).

**Rendering (`workflow_view.rs`):**

Pass `scroll_offset` into `render_workflow_strip`. When rendering parallel groups, skip the first `scroll_offset` steps and render from there. Update the `+ N more…` count accordingly.

**Edge cases:**
- No overflow → scroll events are no-ops.
- Workflow changes (step added/removed) → clamp `scroll_offset` to new bounds.
- Strip not visible (no active workflow) → mouse events ignored.

---

### J. Init command parity

**Problem:** The `init` command in new-amux (`src/engine/init/mod.rs`) diverges from old-amux (`oldsrc/commands/init_flow.rs`) in several user-facing ways.

#### J.1 Conditional work-items setup

**Old-amux** (`oldsrc/commands/init_flow.rs` ~line 965): Work-items setup is offered only when the aspec directory doesn't exist AND work items aren't already configured. This prevents a redundant prompt on re-init.

**New-amux** (`src/engine/init/mod.rs` `AwaitingWorkItemsDecision` phase): Always offers work-items setup unconditionally.

**Fix:** Guard the `AwaitingWorkItemsDecision` phase:

```rust
InitPhase::AwaitingWorkItemsDecision => {
    let aspec_exists = self.git_root.join("aspec").exists();
    let already_configured = self.config.work_items.is_some();
    if aspec_exists || already_configured {
        // Skip — work items are already set up
        return Ok(InitPhase::Complete);
    }
    // ... existing prompt logic
}
```

#### J.2 Detailed interactive prompt text

**Old-amux** provides multi-line explanations before each prompt (e.g. explaining what the audit container does, what aspec replacement means). **New-amux** prompts are terse one-liners.

**Fix (`src/frontend/cli/per_command/init.rs`):**

Update the CLI frontend's `ask_replace_aspec()` and `ask_run_audit()` implementations to include explanatory text before the yes/no prompt. Match the wording from `oldsrc/commands/init_flow.rs` lines 92–132.

For the TUI frontend, ensure dialog body text includes the same explanations (these render in the Custom dialog body).

#### J.3 Explicit `.amux/` directory creation

**Old-amux** (`oldsrc/commands/init_flow.rs` ~line 1093): Explicitly calls `std::fs::create_dir_all(&amux_dir)` before writing files into `.amux/`.

**New-amux**: Relies on downstream `write()` calls to succeed, which may fail if `.amux/` doesn't exist yet.

**Fix:** In the `Preflight` phase (`src/engine/init/mod.rs`), ensure `git_root/.amux/` exists:

```rust
InitPhase::Preflight => {
    std::fs::create_dir_all(self.git_root.join(".amux"))?;
    // ... existing preflight logic
}
```

#### J.4 Non-TTY handling consistency

**Old-amux** has systematic non-TTY handling via its `OutputSink` abstraction. **New-amux** only checks `stdin_is_tty()` for the work-items prompt, not for other interactive prompts.

**Fix:** Add TTY guards to `ask_replace_aspec()` and `ask_run_audit()` in the CLI frontend. When stdin is not a TTY, return safe defaults (don't replace aspec, don't run audit) instead of attempting to read from stdin.

**Edge cases:**
- Re-running `init` on an already-initialized repo → old-amux skips most steps; new-amux should match. Verify each phase checks for existing files/config before overwriting.
- `--aspec` flag interaction → aspec replacement prompt should only trigger when the flag is provided AND the folder already exists, matching old-amux.

---

### K. Worktree cancel-cleanup

**Problem:** When the engine returns `WorkflowOutcome::Aborted` (user picked Abort, or Ctrl+C cancel), the worktree should be preserved on disk so the user can resume later. Old-amux's `cancel_workflow_execution` kept the worktree; new-amux's handling needs verification.

**Check and fix (`src/command/commands/` and `src/frontend/tui/per_command/worktree_lifecycle.rs`):**

After the engine returns `Aborted`:
1. Do NOT auto-delete the worktree directory or branch.
2. Log a status message: `"Workflow aborted — worktree preserved at {path}. Run the workflow again to resume."`.
3. If the worktree has uncommitted changes, do NOT prompt to commit/discard — just leave everything as-is.

Match old-amux semantics: the user's partial work from completed steps is valuable and must not be lost on abort.

**Edge cases:**
- Abort during the first step (no completed work yet) → still preserve the worktree. The user chose to create it; they should choose to destroy it.
- Abort via Ctrl+C on the `WorkflowCancelConfirm` dialog → same preservation semantics.

---

### L. Documentation refresh

**Problem:** The user-facing docs in `docs/` don't cover several TUI features that were added or changed across recent work items, and this work item adds more.

**Scope:** Update `docs/` (create or update the appropriate files — likely `docs/tui.md` or equivalent) to cover:

- All keybindings: Ctrl+W (workflow control, mid-step interrupt), Ctrl+M (cycle container), Ctrl+T (new tab), Ctrl+A/D (tab switch), Ctrl+, (config), `[d]` toggle, `q` to quit.
- The Workflow Control Board: between-step and mid-step variants, what each action does.
- The yolo countdown: what it is, how to dismiss (Esc), how to open the full WCB (Ctrl+W).
- The auto-disable-for-step toggle: what `[d]` does and how it affects auto-advance.
- How to read the workflow strip: status glyphs, step boxes, parallel-group stacking, stuck indicators.
- The lightweight step-confirm dialog and when it appears vs the full WCB.
- Container overlay: maximized/minimized/hidden states, scrollback, stats display.
- The `init` command: what each prompt means and what the defaults are.

**Do not** create work-item-specific documentation. All docs must be user-facing guides.

---

## Edge Case Considerations

(Sub-item-specific edge cases are documented inline above. The following are cross-cutting concerns.)

- **Shared-state races:** Several sub-items add fields to shared `Arc<Mutex<_>>` state (e.g. `stuck` on `WorkflowStepView`, `last_available_actions` on `Tab`). All mutations must hold the lock for the minimum duration — compute outside the lock, then write. Never hold two locks simultaneously.
- **Backwards-compatible persistence:** Sub-item A adds `StepCompletionMode` to the persisted workflow state. The schema version must bump and a migration must default old-format steps to `ContainerExited`. Existing on-disk state files must not break.
- **Keyboard conflicts:** Ctrl+W is already wired. New dialog keybindings (Enter/Esc in `WorkflowStepConfirm`) must not conflict with existing global bindings. Register all new keybindings in `keymap.rs`, not scattered in `mod.rs`.
- **Terminal size edge cases:** Horizontal scroll (E.1), middle ellipsis (F.1), and mouse hit-testing (I) all depend on accurate width calculations. Use `unicode_width` for grapheme-aware width, not `str::len()`.
- **No regressions in headless/CLI:** Sub-items that extend traits (A, B) use default implementations. CLI and headless frontends must not require changes unless explicitly stated.

## Test Considerations

### Per sub-item:

**A. Mid-step control board (engine tests):**
- `open_control_board_mid_step_does_not_cancel_container`: drive engine with a fake factory whose step runs forever; send `OpenControlBoard`; verify `cancel()` was NOT called before `user_choose_next_action` returns.
- `mid_step_dismiss_resumes_waiting_on_step`: open WCB mid-step, user picks Dismiss (Esc); verify the step continues running undisturbed and the engine resumes waiting on it.
- `mid_step_continue_in_current_container_queues_prompt`: open WCB mid-step, user picks ContinueInCurrentContainer with a prompt; verify `inject_prompt()` called on the live execution and step keeps running.
- `mid_step_restart_cancels_then_re_runs`: open WCB, pick Restart; verify `cancel()` called ONLY after selection, then fresh container spawned for same step.
- `mid_step_advance_cancels_then_marks_force_succeeded`: pick LaunchNext; verify cancel, then persisted state has `UserForcedAdvance`.
- `mid_step_cancel_to_previous_rewinds`: pick CancelToPreviousStep; verify cancel, then both steps reset to Pending.
- `step_completes_naturally_while_wcb_open`: step finishes while dialog is up; verify engine detects completion and handles the user's (now stale) action gracefully.
- `resume_from_force_succeeded_step_does_not_re_run`: write state with force-succeeded step, resume, verify skip.

**A. Mid-step control board (TUI tests):**
- `ctrl_w_with_no_workflow_pushes_status_bar_message`: no workflow → hint shown, no dialog.
- `ctrl_w_between_steps_uses_cached_actions`: cached actions + no Running → local WCB.
- `ctrl_w_during_running_step_sends_control_board_request`: verify `control_board_tx` receives message.
- `mid_step_wcb_renders_action_consequence_hints`: verify DarkGray sub-bullets present ("kills running container", "queues message to running agent", "step keeps running").

**B. Auto-disabled:**
- `should_auto_advance_returns_false_for_disabled_step`: verify trait method.
- `engine_skips_yolo_countdown_when_step_disabled`: verify engine falls through to interactive prompt.

**C. Stuck visual:**
- `report_step_stuck_sets_stuck_flag_on_view`: verify shared state updated.
- `strip_renders_warning_glyph_for_stuck_step`: verify rendered output contains `⚠️`.

**D. PTY resize:**
- `cycle_to_maximized_sends_resize`: verify `container_resize_tx` receives new dimensions.
- `cycle_to_hidden_does_not_send_resize`: verify no send.

**E. Command box:**
- `horizontal_scroll_shows_cursor_in_view`: long input → cursor visible, prefix scrolled.
- `ghost_text_shown_when_empty_and_idle`: verify DarkGray hint rendered.
- `ghost_text_hidden_when_typing`: input non-empty → no hint.

**F. Suggestions row:**
- `long_path_truncated_with_middle_ellipsis`: path > width → contains `…`.
- `flag_hint_rendered_with_em_dash`: suggestion with known flag → hint appended.

**G. ConfigShow:**
- `enter_on_read_only_shows_toast`: verify status_bar.text updated.

**H. StepConfirm:**
- `simple_advance_shows_lightweight_dialog`: single next step → StepConfirm, not WCB.
- `ctrl_w_in_step_confirm_escalates_to_wcb`: verify dialog switch.
- `parallel_fan_out_falls_through_to_wcb`: multiple next steps → full WCB.

**I. Mouse scroll:**
- `scroll_down_reveals_hidden_parallel_steps`: overflow → scroll → new steps visible.
- `scroll_clamped_at_bounds`: scroll past end → no crash, stays at max.

**J. Init parity:**
- `work_items_setup_skipped_when_aspec_exists`: init with aspec dir → no prompt.
- `work_items_setup_skipped_when_already_configured`: init with existing config → no prompt.
- `non_tty_returns_safe_defaults`: pipe stdin → no prompts, defaults applied.
- `explicit_amux_dir_created_in_preflight`: verify `.amux/` exists after preflight.

**K. Worktree cancel:**
- `abort_preserves_worktree_on_disk`: abort workflow → worktree dir still exists.
- `abort_does_not_prompt_to_commit`: abort → no commit dialog shown.

### Integration tests (`tests/`):

- End-to-end: run a two-step workflow with a slow first step, send Ctrl+W mid-step, verify WCB opens and step continues running, pick Dismiss (Esc), verify step still running.
- End-to-end: same setup, open WCB mid-step, pick Advance, verify container killed THEN second step runs in a fresh container.
- End-to-end: same setup, open WCB mid-step, pick ContinueInCurrentContainer with a prompt, verify prompt queued and step continues.
- End-to-end: same setup, pick Restart, verify container killed then two container launches total for the first step.
- End-to-end: three-step workflow, open WCB mid-third-step, pick CancelToPrevious, verify container killed then second step re-runs.
- End-to-end: run `init` in a fresh repo, verify all files and directories created match old-amux output.
- Persistence: kill amux while mid-step WCB is open, restart, verify resume offers sane recovery (step was still Running, so `interrupted_running_steps()` handles it).

## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Layer rules: the TUI must not call runtime/container methods directly. The control-board channel goes into the engine; the engine owns all container lifecycle decisions (cancel only happens when the user's chosen action requires it).
- The `WorkflowFrontend` trait is the only TUI ↔ engine boundary for workflow concerns. Don't add ad-hoc shortcuts.
- State persistence schema changes bump the version and ship a migration; do not break existing on-disk state files.
- Keyboard bindings: register all new bindings in `keymap.rs` — no scattered `KeyCode::Char(...)` matches.
- Use `oldsrc/` as a read-only reference for parity verification. Do not modify `oldsrc/`.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0074 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
