# Work Item: Task

Title: Ground-Up Rewrite of WorkflowEngine, Frontend Traits, Yolo Mode, and WCB
Issue: ENG-1, TUI-1 (recurring, architectural)

## Summary:
- **Delete** the entire `WorkflowEngine`, all workflow state stored in TUI frontend modules, all workflow-related keyboard handling, the `WorkflowFrontend` trait, and all yolo/WCB/stuck-detection logic across every frontend.
- **Rewrite from scratch** with the engine as the single source of truth for ALL workflow state, the frontend as a pure I/O layer, and channels as the sole communication mechanism.
- **Preserve** the worktree lifecycle (`WorktreeLifecycle::prepare` / `finalize`), the data layer (`WorkflowState`, `WorkflowDag`, `WorkflowStateStore`, `workflow_definition`, `workflow_prompt_template`), and the command-layer orchestration in `exec_workflow.rs`.
- **Model after old-amux's architecture** (`oldsrc/`) where a single event loop tick drives all state transitions, stuck detection, and yolo countdowns from one authoritative location.

## User Stories

### User Story 1:
As a: user running `exec workflow --yolo`

I want to: have stuck containers automatically trigger a yolo countdown, see the countdown (or a flashing tab if it's a background tab), and have the workflow auto-advance when the countdown expires

So I can: run multi-step workflows unattended with confidence that stuck steps will be automatically advanced

### User Story 2:
As a: user running any workflow (yolo or non-yolo)

I want to: press Ctrl-W at ANY time — during a running container, between steps, after the last step, during a yolo countdown, during any dialog — and ALWAYS see the Workflow Control Board with correct available actions

So I can: have full control over workflow execution at all times without fighting the UI

### User Story 3:
As a: user who cancels a yolo countdown

I want to: have the countdown automatically restart if the container becomes stuck again, without the workflow being affected in any way

So I can: temporarily interact with a stuck container and then let yolo resume automatically

## Implementation Details:

### Phase 0: Deletion

Delete the following files and all workflow-related code within them. This is not a refactor — it is a ground-up rewrite.

**Engine — DELETE entirely and rewrite:**
- `src/engine/workflow/mod.rs` — the entire `WorkflowEngine` struct and impl
- `src/engine/workflow/frontend.rs` — the entire `WorkflowFrontend` trait
- `src/engine/workflow/timing.rs` — timing constants (will be re-created)

**Engine — PRESERVE (do not delete):**
- `src/engine/workflow/actions.rs` — `NextAction`, `AvailableActions`, `YoloTickOutcome`, etc. Keep these types; refine if needed.
- `src/engine/workflow/factory.rs` — `ContainerExecutionFactory` trait. Keep as-is.

**TUI Frontend — DELETE all workflow-related code in:**
- `src/frontend/tui/per_command/workflow_frontend.rs` — entire file, rewrite
- `src/frontend/tui/per_command/exec_workflow.rs` — workflow-related portions
- `src/frontend/tui/dialogs/mod.rs` — all workflow dialog types (WCB, YoloCountdown, StepConfirm, StepError, CancelConfirm)
- `src/frontend/tui/tabs.rs` — DELETE: `yolo_dismissed_at`, `WorkflowViewState`, and any workflow execution state. KEEP: stuck detection fields (`stuck`, `last_output_time`, `last_user_activity_time`), `recompute_stuck()`, `is_stuck()` — these stay because stuck detection is a frontend responsibility
- `src/frontend/tui/app.rs` — all stuck-to-yolo-countdown flow, `ControlBoardRequest` sending, yolo backoff logic
- `src/frontend/tui/mod.rs` — all `Action::WorkflowControl` handling, yolo state management, control board channel wiring
- `src/frontend/tui/workflow_view.rs` — preserve the rendering logic but remove any state it reads from `Tab` (it should read from engine-provided state only)
- `src/frontend/tui/render.rs` — workflow-related dialog rendering
- `src/frontend/tui/keymap.rs` — yolo-related key handling
- `src/frontend/tui/hints.rs` — yolo-related hints
- `src/frontend/tui/command_frontend.rs` — workflow delegation

**CLI Frontend — DELETE workflow-related code in:**
- `src/frontend/cli/per_command/workflow_frontend_marker.rs` — entire file, rewrite
- `src/frontend/cli/per_command/exec_workflow.rs` — workflow-related portions
- `src/frontend/cli/command_frontend.rs` — workflow delegation

**Headless Frontend — DELETE workflow-related code in:**
- `src/frontend/headless/command_frontend.rs` — workflow-related portions

**Data layer — PRESERVE entirely:**
- `src/data/workflow_state.rs` — `WorkflowState` struct (canonical, serializable)
- `src/data/workflow_state_store.rs` — persistence
- `src/data/workflow_definition.rs` — `Workflow`, `WorkflowStep`
- `src/data/workflow_dag.rs` — DAG validation and traversal
- `src/data/workflow_prompt_template.rs` — prompt substitution
- `src/data/fs/workflow_state.rs`, `src/data/fs/workflow_dirs.rs` — file I/O

**Command layer — PRESERVE but adapt:**
- `src/command/commands/exec_workflow.rs` — preserve worktree lifecycle integration, adapt engine construction
- `src/command/commands/worktree_lifecycle.rs` — PRESERVE ENTIRELY, DO NOT TOUCH

### Phase 1: New WorkflowEngine Architecture

The new engine must follow these architectural rules absolutely:

#### Rule 1: Engine Owns ALL Workflow State

The `WorkflowEngine` is the single source of truth for:
- Step states (Pending/Running/Succeeded/Failed/Cancelled/Skipped)
- Current step name and execution
- Yolo countdown state (started_at, remaining, expired)
- Whether the engine is currently responding to a stuck notification
- WCB availability and computed actions
- Which dialog/overlay the frontend should show

**No workflow state in frontend code. ZERO. NONE.**

The `Tab` struct in the TUI may contain stuck-detection fields (`last_output_time`, `stuck` flag) because stuck detection is a frontend responsibility (see Rule 2). But it must NOT contain: `yolo_dismissed_at`, `yolo_countdown_started_at`, `WorkflowViewState`, step states, workflow progress, or any other workflow *execution* state. The tab is a display surface for execution state. The engine tells it what to display via trait calls.

#### Rule 2: Stuck Detection Stays in the Frontend, Response Lives in the Engine

The frontend (TUI, CLI, headless) is responsible for DETECTING that a container is stuck (no PTY output for `STUCK_TIMEOUT`). The engine is responsible for RESPONDING to that notification.

Flow:
1. Frontend detects container is stuck (no output for 30s)
2. Frontend sends `EngineRequest::StepStuck` on the engine channel
3. Engine receives the notification and responds:
   - If `--yolo`: engine starts yolo countdown internally, calls `frontend.yolo_countdown_started(step_name)`
   - If not `--yolo`: engine computes available actions, calls `frontend.show_workflow_control_board(state, actions)`
4. Frontend detects container is no longer stuck (new output arrived)
5. Frontend sends `EngineRequest::StepUnstuck` on the engine channel
6. Engine cancels any active yolo countdown, calls `frontend.yolo_countdown_finished(step_name)`. Frontend already cleared its own stuck indicator when it detected new output.

The frontend decides "is this stuck?" — the engine decides "what do we do about it?"

The engine must NOT ignore `StepStuck` based on backoff timers, dialog state, or any other condition. Every `StepStuck` notification is actionable. If a yolo countdown was previously cancelled and the frontend sends `StepStuck` again, the engine starts a new countdown.

#### Rule 3: Yolo Is a Step Transition Type, Not a Mode

Yolo countdown is triggered by stuck detection. Period.

```
Container running → no output for 30s → STUCK
STUCK + yolo=true → start 60s countdown
Countdown expires → engine kills container, marks step Succeeded, advances workflow
Countdown cancelled (user Esc) → countdown stops, step keeps running
Step becomes stuck AGAIN → countdown restarts from 60s
New output arrives → UNSTUCK, countdown cancelled automatically
```

Canceling a yolo countdown:
- Does NOT cancel the workflow
- Does NOT cancel the step
- Does NOT prevent future yolo countdowns on the same step
- Simply means "give me more time with this container"

The yolo countdown restarts every time the step re-enters the stuck state. There is no "dismissed" backoff for yolo. If the user presses Esc and the container immediately re-stucks, the countdown starts again. The user can press Esc again. This is correct behavior.

#### Rule 4: WCB Is Always Accessible

The Workflow Control Board must be showable when:
- A container is running (mid-step)
- A container just exited (between steps)
- The last step just completed (workflow about to finish)
- A yolo countdown is active (Ctrl-W replaces the countdown with WCB)
- Any other dialog is open (Ctrl-W dismisses it and shows WCB)
- No dialog is open

There are ZERO conditions under which Ctrl-W should be silently ignored when a workflow is active. The only precondition is: `workflow.is_some()`.

The engine always has the state to compute available actions. "Mid-step" is not a special case — it's just another set of available actions (with `can_dismiss = true`).

#### Rule 5: No "Mid-State" Special Casing

The engine's `compute_available_actions()` looks at its current state and returns what's possible. It does not care whether it's "between steps" or "mid-step" or "post-workflow" — those are just different configurations of the same state variables.

```rust
fn compute_available_actions(&self) -> AvailableActions {
    let has_next = self.dag.next_ready(&self.state).is_some();
    let is_last = /* ... */;
    let step_running = self.current_execution.is_some();
    let same_agent = /* ... */;

    AvailableActions {
        can_launch_next: has_next && !step_running,
        can_continue_in_current_container: has_next && step_running && same_agent,
        can_restart_current_step: self.current_step_name.is_some(),
        can_cancel_to_previous_step: /* not first step */,
        can_finish_workflow: is_last,
        can_pause: true,
        can_abort: true,
        can_dismiss: step_running,
        // ... reasons for unavailable actions
    }
}
```

No `if is_mid_step { ... } else { ... }` branching. One function, one truth.

#### Rule 6: Per-Tab Engine Isolation (Multi-Workflow Support)

Multiple tabs can run independent workflows simultaneously. The TUI must route all signals — Ctrl-W, StepStuck, StepUnstuck — to the **correct tab's engine**, never to a global or shared channel.

**Ownership boundary: The TUI/Tab NEVER owns a WorkflowEngine instance.**

The ownership chain is: Tab → Dispatch → Command (`ExecWorkflowCommand`) → `WorkflowEngine`. The engine lives inside the command's async task, which runs on the tokio runtime. The tab only holds a **channel sender** (`engine_tx`) — a lightweight, cloneable handle that lets the TUI send `EngineRequest` messages to the engine without owning or referencing it.

**Architecture:**

Each `Tab` stores a per-tab engine channel slot:
```rust
/// Per-tab sender to the tab's WorkflowEngine. `None` when no workflow is running.
/// Set by the engine (via the frontend trait's `set_engine_sender()`) at engine startup.
/// Used by the TUI event loop to send EngineRequests.
/// The Tab does NOT own the WorkflowEngine — only this channel sender.
engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>
```

This follows the existing pattern from current new-amux (`control_board_tx_shared`). The key invariants:

1. **Command owns the engine, tab owns a channel sender.** Each tab's Dispatch runs a command (e.g., `ExecWorkflowCommand`) in its own async task. If that command is a workflow, it constructs and owns the `WorkflowEngine`. The engine creates its `EngineRequest` channel, keeps the receiver, and publishes the sender to the tab's `engine_tx` slot via `frontend.set_engine_sender(tx)`. The tab only ever touches the sender — it cannot call engine methods, inspect engine state, or hold a reference to the engine.

2. **Ctrl-W routes to the active tab's engine.** The TUI event loop reads `active_tab().engine_tx` and sends `OpenControlBoard` on that sender. It never broadcasts to all tabs.

3. **Stuck detection routes to each tab's own engine.** The `tick_all_tabs()` loop iterates over ALL tabs, runs each tab's stuck detection independently, and sends `StepStuck` / `StepUnstuck` on that **specific tab's** `engine_tx`. A tab in the background can detect stuck and notify its own engine — the engine responds (e.g., starts yolo countdown) even though the tab is not active.

4. **No cross-tab interference.** Tab A's yolo countdown is completely independent of Tab B's workflow. Switching from Tab A to Tab B does not affect Tab A's engine. Tab A's engine keeps ticking its countdown; Tab A's header flashes in the background.

5. **Channel lifecycle.** The `engine_tx` slot is `None` when:
   - No command is running in the tab
   - The running command is not a workflow
   - The workflow engine has finished and dropped the receiver
   
   The TUI checks for `Some(tx)` before sending. If `None`, Ctrl-W and stuck detection are no-ops for that tab.

6. **Engine cleanup.** When the engine's async task completes (workflow finished, aborted, or errored), the receiver is dropped. Any subsequent `send()` from the TUI returns `Err` (channel closed). The TUI should handle this gracefully — a closed channel means the workflow is done, not an error.

#### Rule 7: Engine Emits Status Messages for All State Transitions

Every workflow state transition must produce a `UserMessage` via the `WorkflowFrontend`'s `write_message` method (from the `UserMessageSink` trait). These messages appear in the TUI's scrollable execution log and are queued for CLI output after each container exits.

State transitions that require messages include:
- Workflow started / resumed
- Step launched (with agent name and model)
- Step completed (success or failure, with exit code)
- Step failure response (retry, pause, abort)
- Yolo countdown started / cancelled / expired (auto-advance)
- Yolo countdown recovered (container produced output)
- WCB action taken (user chose an action)
- Workflow completed / aborted
- Steps skipped due to dependency failures

The engine uses the `write_message` method directly (not the `info()` / `warning()` / `success()` convenience methods) because those convenience methods have `Self: Sized` bounds that prevent calling them on `Box<dyn WorkflowFrontend>`. Private helpers (`msg_info`, `msg_warning`, `msg_success`) on the `WorkflowEngine` struct wrap the `write_message` call for ergonomics.

#### Rule 8: Frontend Stores No Workflow Execution State

The `WorkflowViewState` and `WorkflowStepView` structs in the TUI are **display-only projections**. They contain only what the renderer needs to draw the workflow strip:
- Step name, status string, agent, model, dependencies (for layout)
- Current step name (for highlighting)

They must NOT contain:
- `auto_disabled` / per-step auto-advance flags (engine state)
- `stuck` flag per step (engine response state — distinct from the Tab-level container stuck detection)
- Any field that the TUI would read to make workflow decisions

The engine writes these projections via `report_workflow_progress`. The renderer reads them. No other code path touches them.

### Phase 2: New WorkflowFrontend Trait

The trait must be redesigned to be **engine-driven, not frontend-driven**.

```rust
pub trait WorkflowFrontend: Send {
    // === Engine-driven display commands ===

    /// Engine tells frontend to show the WCB with these actions.
    /// Frontend collects user input and returns the chosen action.
    /// This is a BLOCKING call — engine waits for the user's choice.
    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError>;

    /// Engine tells frontend to show the yolo countdown overlay.
    /// Called repeatedly (every 100ms) with remaining time.
    /// Frontend returns whether to Continue, Cancel, or AdvanceNow.
    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError>;

    /// Engine tells frontend: yolo countdown just started for this step.
    /// Frontend should show the countdown dialog (active tab) or flash
    /// the tab yellow/purple (background tab).
    fn yolo_countdown_started(&mut self, step_name: &str);

    /// Engine tells frontend: yolo countdown finished (expired, cancelled,
    /// or step completed). Frontend dismisses dialog / resets tab style.
    fn yolo_countdown_finished(&mut self, step_name: &str);

    // === Status reporting (fire-and-forget) ===

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus);
    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput);
    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);
    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]);
    fn report_step_interactive_launch(&mut self, step: &WorkflowStep, agent: &str, model: Option<&str>);

    // === User decisions (blocking) ===

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;
    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError>;

    // === Channel setup ===

    /// Called by the engine after creating its EngineRequest channel.
    /// The frontend stores the sender in the tab's `engine_tx` slot so the
    /// TUI event loop can route Ctrl-W and stuck notifications to this
    /// specific engine instance. Each tab gets its own sender — this is
    /// how the TUI disambiguates between multiple concurrent workflows.
    fn set_engine_sender(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>,
    );
}
```

Key changes from current trait:
1. **`show_workflow_control_board`** replaces `user_choose_next_action` — name makes the intent clear
2. **`yolo_countdown_started` / `yolo_countdown_finished`** — explicit lifecycle callbacks instead of relying on frontend to track state
3. **Removed** `show_stuck_indicator` / `clear_stuck_indicator` — stuck detection and rendering is the frontend's job; the engine only responds to stuck notifications via the `EngineRequest` channel
4. **Removed** `set_control_board_sender` — the engine no longer needs the frontend to hold a channel sender; the engine receives input via a different mechanism (see Phase 3)
5. **Removed** `should_auto_advance` — the engine decides this internally from its own per-step state
6. **Removed** `reset_yolo_initialized` / `clear_yolo_state` — engine manages its own state lifecycle
7. **Removed** `report_step_stuck` / `report_step_unstuck` — stuck detection lives entirely in the frontend; the frontend notifies the engine via `EngineRequest::StepStuck` / `StepUnstuck` channels, not the other way around

### Phase 3: Channel Architecture

```
┌──────────────────────┐
│      TUI Event Loop   │
│                       │
│  Tab 0  Tab 1  Tab 2  │   Each tab has its own engine_tx: Arc<Mutex<Option<Sender>>>
│   │      │      │     │
│   ▼      ▼      ▼     │
│  ┌──┐  ┌──┐  ┌──┐    │   Ctrl-W  → active_tab().engine_tx.send(OpenControlBoard)
│  │tx│  │tx│  │tx│    │   Stuck   → tab[i].engine_tx.send(StepStuck)     (per-tab)
│  └┬─┘  └┬─┘  └┬─┘    │   Unstuck → tab[i].engine_tx.send(StepUnstuck)  (per-tab)
└───┼─────┼─────┼───────┘
    │     │     │          mpsc channels (one per tab)
    ▼     ▼     ▼
┌──────┐ ┌──────┐ ┌──────┐
│ WE 0 │ │ WE 1 │ │ WE 2 │   Each WorkflowEngine owned by its Command's async task
│      │ │      │ │      │   (Tab → Dispatch → Command → WorkflowEngine)
│      │ │      │ │      │   Owns rx, consumes EngineRequests in select! loop
│      │ │      │ │      │   Calls trait fns on its own WorkflowFrontend impl
└──────┘ └──────┘ └──────┘
```

**Engine-bound channel (`EngineRequest`):**
```rust
pub enum EngineRequest {
    /// User pressed Ctrl-W. Engine should show WCB.
    OpenControlBoard,
    /// Frontend detected that the current step's container is stuck
    /// (no PTY output for STUCK_TIMEOUT). Engine responds: if --yolo,
    /// start yolo countdown; if not --yolo, open WCB.
    StepStuck,
    /// Frontend detected that the container is no longer stuck (new
    /// PTY output arrived). Engine cancels any active yolo countdown.
    StepUnstuck,
}
```

The engine runs its own async loop consuming these events alongside its step execution:

```rust
loop {
    tokio::select! {
        // Step container exit
        exit = step_execution.wait() => { /* handle step completion */ }

        // Frontend request
        Some(req) = engine_rx.recv() => {
            match req {
                EngineRequest::OpenControlBoard => {
                    // Cancel yolo countdown if active
                    // Compute available actions
                    // Call frontend.show_workflow_control_board()
                    // Execute chosen action
                }
                EngineRequest::StepStuck => {
                    if self.yolo {
                        self.frontend.yolo_countdown_started(&step_name);
                        self.run_yolo_countdown().await;
                    } else {
                        let actions = self.compute_available_actions();
                        let choice = self.frontend.show_workflow_control_board(
                            &self.state, &actions)?;
                        self.execute_action(choice).await?;
                    }
                }
                EngineRequest::StepUnstuck => {
                    self.cancel_yolo_countdown();
                    self.frontend.yolo_countdown_finished(&step_name);
                }
            }
        }
    }
}
```

### Phase 4: TUI Frontend Implementation

The TUI implementation of `WorkflowFrontend` is a thin I/O adapter:

1. **`show_workflow_control_board`**: Opens a WCB dialog, blocks on user input (via `std::sync::mpsc` dialog channel), returns the chosen `NextAction`. No state tracking — the dialog is ephemeral.

2. **`yolo_countdown_tick`**: Updates the shared yolo display state (remaining time), checks if user pressed Esc (via dialog response channel), returns the outcome. No countdown logic — just display and input collection.

3. **`yolo_countdown_started`**: If active tab → opens yolo countdown dialog. If background tab → sets tab to flash yellow/purple.

4. **`yolo_countdown_finished`**: Dismisses dialog, resets tab visual state.

**What the TUI event loop does for workflows:**
- Receives Ctrl-W → sends `EngineRequest::OpenControlBoard` on the **active tab's** `engine_tx` channel
- Receives PTY output → forwards to vt100 parser, updates `last_output_time`, runs stuck detection
- Detects stuck (no PTY output for 30s) → sends `EngineRequest::StepStuck` on **that specific tab's** `engine_tx`, renders stuck indicator on that tab
- Detects unstuck (PTY output resumes after stuck) → sends `EngineRequest::StepUnstuck` on **that specific tab's** `engine_tx`, clears stuck indicator on that tab
- `tick_all_tabs()` iterates ALL tabs for stuck detection — each tab notifies its own engine independently, even background tabs
- Receives tab switch → no engine notification needed (stuck detection is frontend-local, per-tab)

**What the TUI event loop does NOT do for workflows:**
- Track yolo countdown state (engine owns this)
- Decide when to show WCB (engine decides)
- Decide when to start yolo countdown (engine decides)
- Store any workflow *execution* state in `Tab` (no step states, no yolo state, no WCB state)
- Gate Ctrl-W behind any condition other than "workflow exists"

**What the TUI event loop DOES own:**
- Stuck detection (tracking `last_output_time` and comparing against `STUCK_TIMEOUT`)
- Rendering the stuck indicator on the tab
- Sending `StepStuck` / `StepUnstuck` notifications to the engine
- Rendering the workflow state strip based on workflow state recieved from the workflow engine **the state itself is still owned by the engine, the TUI just renders the strip based on it**

### Phase 5: Keyboard Handling

Ctrl-W handling is trivial in the new architecture:

```rust
// In TUI event loop — Ctrl-W routes to the ACTIVE TAB's engine
Action::WorkflowControl => {
    // Lock the active tab's engine_tx slot
    if let Ok(guard) = app.active_tab().engine_tx.lock() {
        if let Some(ref tx) = *guard {
            // Dismiss any open dialog first
            if app.active_dialog.is_some() {
                app.dismiss_active_dialog();
            }
            let _ = tx.send(EngineRequest::OpenControlBoard);
        }
    }
    // If no workflow_engine_tx, there's no workflow — ignore silently
}
```

That's it. No guards. No mid-step checks. No dialog-type checks. No yolo state checks. If there's an engine channel, send the request. The engine decides what to do.

### Phase 6: Yolo Mode (Complete Specification)

#### Yolo Lifecycle for a Single Step:

```
1. Engine launches container for step N
2. Frontend initializes stuck detection: last_output_time = now()
3. Engine enters select! loop (see Phase 3), frontend runs its event loop
4. Container produces output → frontend updates last_output_time, renders output
   - If was stuck: frontend sends EngineRequest::StepUnstuck, clears stuck indicator
5. 30 seconds pass with no output → frontend detects stuck
   - Frontend renders stuck indicator on tab
   - Frontend sends EngineRequest::StepStuck on the engine channel
6. Engine receives StepStuck, calls frontend.yolo_countdown_started(step_name)
7. Engine enters yolo countdown loop:
   a. Every 100ms: call frontend.yolo_countdown_tick(step_name, remaining, total)
   b. If tick returns Continue → countdown keeps going (this includes tab-switch: dialog
      closes but engine keeps ticking, frontend returns Continue not Cancel)
   c. If tick returns Cancel → user explicitly pressed Esc, cancel countdown, go to step 8
   d. If tick returns AdvanceNow → go to step 9
   e. If countdown expires (60s) → go to step 9
   f. If EngineRequest::StepUnstuck arrives → container recovered, go to step 10
   g. If EngineRequest::OpenControlBoard arrives → go to step 11
   h. If container exits → go to step 12
8. CANCELLED: Engine calls frontend.yolo_countdown_finished(step_name)
   - Step keeps running. Engine returns to select! loop (step 3).
   - If container becomes stuck again → frontend re-sends StepStuck → yolo restarts.
   - There is NO backoff. NO "already dismissed" flag. Stuck = yolo, always.
9. EXPIRED/ADVANCED: Engine calls frontend.yolo_countdown_finished(step_name)
    - Engine kills container (graceful stop, then force if needed)
    - Engine marks step Succeeded (yolo auto-advance is an intentional success)
    - Engine advances to next step (or finishes workflow if last step)
10. RECOVERED: Engine calls frontend.yolo_countdown_finished(step_name)
    - Engine returns to select! loop (step 3)
    - Frontend already cleared its own stuck indicator when it sent StepUnstuck
11. CTRL-W DURING COUNTDOWN: Engine calls frontend.yolo_countdown_finished(step_name)
    - Engine computes available actions
    - Engine calls frontend.show_workflow_control_board(state, actions)
    - User chooses action → engine executes it
    - If action is Dismiss → engine returns to select! loop. If step re-stucks, frontend sends StepStuck, yolo restarts.
12. CONTAINER EXITED DURING COUNTDOWN: Engine calls frontend.yolo_countdown_finished(step_name)
    - Engine processes exit normally (success/failure handling)
    - Countdown is moot — container already exited
```

#### Tab Switching During Yolo Countdown:

**CRITICAL: Switching tabs does NOT cancel a yolo countdown. It only backgrounds it.**

The yolo countdown is engine-driven. The engine keeps ticking regardless of which tab
the user is looking at. The frontend adapts its rendering:

**Active tab → user switches away (Ctrl-A / Ctrl-D):**
- Yolo dialog closes on the tab being left
- Tab header begins flashing yellow/purple (background countdown indicator)
- Engine keeps calling `yolo_countdown_tick` — frontend returns `Continue` (not `Cancel`)
- The `yolo_countdown_tick` implementation must distinguish between "user pressed Esc" (= Cancel)
  and "tab lost focus" (= Continue, just stop showing the dialog)

**Background tab with active countdown → user switches TO it:**
- TUI immediately shows the yolo countdown dialog with the current remaining time
- User can press Esc to cancel, or let it expire, or switch away again
- Tab header stops flashing (dialog is now visible instead)

**Countdown starts while tab is already in background:**
- `yolo_countdown_started` → TUI begins flashing the tab header yellow/purple (no dialog)
- `yolo_countdown_tick` → TUI updates internal countdown value (for when user switches to tab)
- `yolo_countdown_finished` → TUI stops flashing, resets tab header

**Countdown expires while tab is in background:**
- Engine calls `yolo_countdown_finished` → TUI stops flashing
- Engine kills container, advances workflow — all engine-side, no frontend involvement needed
- When user eventually switches to the tab, they see the next step (or workflow completion)

**Summary of what cancels vs. does not cancel a yolo countdown:**

| User action | Cancels countdown? |
|---|---|
| Press Esc while yolo dialog visible | YES |
| Press Ctrl-W (opens WCB instead) | YES |
| Press Ctrl-A / Ctrl-D (switch tabs) | NO — backgrounds it |
| New PTY output (StepUnstuck) | YES |
| Container exits | YES (moot) |

### Phase 7: Preserve Worktree Lifecycle

The worktree lifecycle code is NOT part of this rewrite. It lives at a higher layer (command layer) and must be preserved exactly:

- `src/command/commands/worktree_lifecycle.rs` — DO NOT MODIFY
- `src/command/commands/exec_workflow.rs` — preserve the worktree setup/teardown flow around engine construction. The new engine plugs into the same `exec_workflow.rs` orchestration.
- All `WorktreeLifecycleFrontend` trait implementations — DO NOT MODIFY

The engine receives a `Session` (possibly re-rooted to a worktree path) and runs steps in it. It does not know or care about worktrees.

### Phase 8: Reference Old-Amux Patterns

Consult `oldsrc/` for architectural guidance:

- `oldsrc/workflow/mod.rs` — `WorkflowState` as pure state machine with persistent JSON state
- `oldsrc/tui/state.rs` — `TabState` with `is_stuck()`, `tick()`, `yolo_countdown_started_at` as single authoritative timer
- `oldsrc/tui/input.rs` — Ctrl-W handling (lines 332-347): guard only on `workflow.is_some()` and `dialog == Dialog::None` (but in new-amux, don't even check for dialog — dismiss it)
- `oldsrc/tui/mod.rs` — main event loop with `tick_all()` running every 16ms, yolo expiry check at lines 267-289

Key old-amux patterns to adopt:
1. **Single-authoritative-timer**: `yolo_countdown_started_at` is the only source of truth for countdown
2. **Stuck suppression layering**: output-based, user-activity-based (but NO dialog-backoff-based — that was a mistake)
3. **Non-blocking event loop**: 16ms tick with channel draining
4. **Modal-centric control flow**: dialogs gate decisions, but Ctrl-W always wins

Key old-amux patterns to IMPROVE upon:
1. Old-amux stored workflow state in `TabState` — new-amux must keep execution state in the engine only
2. Old-amux had `workflow_stuck_dialog_dismissed_at` backoff — new-amux has NO backoff for yolo (stuck = yolo, always)

Key old-amux patterns to KEEP:
1. Stuck detection in the frontend (TUI tracks `last_output_time`, compares against timeout) — this is correct because the frontend owns the PTY and knows about output timing. The frontend notifies the engine, and the engine decides the response.

## Edge Case Considerations:

1. **Container exits during yolo countdown**: Countdown becomes moot. Engine processes exit normally. Frontend gets `yolo_countdown_finished` then normal step completion.

2. **Ctrl-W during yolo countdown**: Countdown cancelled, WCB shown. If user dismisses WCB (Esc) and step re-stucks, yolo restarts.

3. **Ctrl-W during step failure dialog**: Step failure dialog dismissed, WCB shown. WCB actions include retry/abort.

4. **Ctrl-W when no container running (between steps)**: WCB shows inter-step actions (LaunchNext, Pause, Abort, FinishWorkflow). No Dismiss option.

5. **Ctrl-W after last step exits**: WCB shows FinishWorkflow, Abort. No LaunchNext.

6. **Multiple rapid Ctrl-W presses**: First opens WCB. Subsequent presses while WCB is open are no-ops (WCB is already showing). Engine is blocking on `show_workflow_control_board`, so the channel just buffers — engine will drain on next select loop.

7. **User switches tabs during yolo countdown (Ctrl-A/Ctrl-D)**: Yolo dialog closes but countdown continues in engine (it's engine-driven). Tab header begins flashing yellow/purple. `yolo_countdown_tick` returns `Continue` (NOT `Cancel`). When user switches back, yolo dialog reappears with current remaining time. User can then Esc to cancel, or let it expire, or switch away again. Switching tabs is NOT a cancellation — only explicit Esc is.

8. **Step produces output, then stucks again, then produces output again**: Each output → unstuck → stuck cycle works independently. Yolo countdown starts fresh each time.

9. **Yolo countdown expires on last step**: Engine should still auto-advance (mark step Succeeded) and then present WCB with FinishWorkflow as the primary action. Do NOT silently finish the workflow — let the user confirm or choose Abort.

10. **Network/Docker failure during step kill (yolo expiry)**: Engine attempts graceful stop, then force kill. If both fail, mark step Failed and enter failure handling flow.

11. **Multiple tabs running workflows simultaneously**: Tab A and Tab B each have independent workflow engines. Tab A's yolo countdown does not affect Tab B. Ctrl-W only targets the active tab. Stuck detection fires independently per-tab — Tab A can be stuck while Tab B runs normally. Both tabs' engines respond to their own `StepStuck` independently.

12. **Ctrl-W on a tab with no workflow**: The active tab's `engine_tx` is `None`. Ctrl-W is a silent no-op. No error, no crash.

13. **Workflow finishes while tab is in background**: Engine's async task completes, drops the receiver. The tab's `engine_tx` sender is still `Some(tx)` but sends will return `Err` (closed channel). The TUI should handle this gracefully — treat a closed channel the same as `None` (workflow is done). When the user switches to the tab, they see the final state.

14. **Switching from Tab A (yolo counting down) to Tab B (also yolo counting down)**: Tab A's dialog closes, Tab A starts flashing. Tab B's dialog appears with Tab B's current remaining time. Both countdowns continue independently in their respective engines.

## Test Considerations:

### Unit Tests (engine):
- `compute_available_actions` returns correct flags for every state combination
- `StepStuck` request in yolo mode starts yolo countdown (calls `yolo_countdown_started`)
- `StepStuck` request in non-yolo mode opens WCB (calls `show_workflow_control_board`)
- `StepUnstuck` request during yolo countdown cancels countdown (calls `yolo_countdown_finished`)
- Yolo countdown expires after YOLO_COUNTDOWN_DURATION → step killed, marked Succeeded
- Yolo countdown restarts after cancellation + re-`StepStuck`
- `OpenControlBoard` request during yolo countdown cancels countdown and shows WCB
- `OpenControlBoard` request when no step running returns correct actions
- `OpenControlBoard` request when step running returns `can_dismiss = true`
- Step exit during yolo countdown finishes countdown and processes exit normally
- Engine does NOT ignore `StepStuck` based on backoff, prior dismissal, or any other condition

### Integration Tests:
- Full workflow with 3 steps, yolo mode, simulated stuck detection → auto-advance
- Ctrl-W during running step → WCB → Dismiss → step continues
- Ctrl-W during yolo countdown → WCB → LaunchNext → step killed, next launched
- Yolo cancel → re-stuck → yolo restarts
- Workflow with step failure → retry → success
- Workflow completion → WCB shows FinishWorkflow

### TUI Tests:
- Ctrl-W sends EngineRequest::OpenControlBoard regardless of dialog state
- Stuck detection fires after STUCK_TIMEOUT with no PTY output → sends EngineRequest::StepStuck
- New PTY output after stuck → sends EngineRequest::StepUnstuck, clears stuck indicator
- Re-stuck after unstuck → sends StepStuck again (no backoff)
- yolo_countdown_started opens dialog (active tab) or flashes tab (background)
- yolo_countdown_finished dismisses dialog / stops flash
- Tab switch (Ctrl-A/Ctrl-D) during yolo dialog → dialog closes, tab flashes, `yolo_countdown_tick` returns `Continue` (not `Cancel`)
- Tab switch back to countdown tab → yolo dialog reappears with current remaining time
- Esc during yolo dialog → `yolo_countdown_tick` returns `Cancel` (distinct from tab switch)
- Ctrl-W routes to active tab's `engine_tx` only — not broadcast to all tabs
- Stuck detection in `tick_all_tabs` sends `StepStuck` on each tab's own `engine_tx` independently
- Ctrl-W on tab with no workflow (`engine_tx` is `None`) → silent no-op
- Closed `engine_tx` channel (workflow finished) → handled gracefully, treated as no workflow

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Ensure all three frontends (TUI, CLI, headless) implement the new `WorkflowFrontend` trait
- CLI frontend: blocking stdin prompts for WCB, auto-advance for yolo tick
- Headless frontend: deterministic defaults (always AdvanceNow for yolo, always LaunchNext for WCB)
- Preserve `ContainerExecutionFactory` trait boundary between engine and container runtime

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
