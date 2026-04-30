# Work Item: Enhancement

Title: Stuck and Yolo Improvements
Issue: issuelink

## Summary

Two related UX improvements to stuck-detection and yolo workflow behavior:

1. **Active-tab activity suppression**: Any user activity (keypresses, scrolling) on the currently active tab resets the 10s stuck timer and suppresses stuck indicators, so users are not interrupted by yellow tabs or dialogs while they are actively reading output. This only applies to the active tab; background tabs retain their existing stuck-detection behavior.

2. **Background yolo countdown**: When a background tab is running a yolo-mode workflow and becomes stuck, it begins the 60s yolo countdown and displays visual feedback in the tab bar (alternating yellow/purple with countdown text) instead of a dialog. If the user switches to that tab during the countdown, the yolo dialog opens without resetting the timer. While the yolo dialog is open, Ctrl+A/Ctrl+D close the dialog and revert the tab to background countdown mode. If the countdown expires while the tab is in the background, the workflow auto-advances and the tab returns to its normal color and text.


## User Stories

### User Story 1
As a: user

I want to: continue reading or scrolling through a container's output without the stuck indicator or yolo dialog appearing while I am actively interacting with the tab

So I can: review output without being interrupted by yellow tab colors or modal dialogs triggered by my own activity pausing the container output.

### User Story 2
As a: user

I want to: see a live countdown in the tab bar for any background tab that is waiting on a yolo workflow decision

So I can: monitor and anticipate workflow advancement across multiple tabs without switching away from my current work, and trust that the workflow will auto-advance when the timer expires.

### User Story 3
As a: user

I want to: freely switch between tabs while a yolo dialog is open and have the dialog close and the countdown continue in the background

So I can: attend to other tabs mid-countdown without losing my place in the timer or being forced to resolve the dialog before navigating away.


## Implementation Details

### Data Model Changes (`src/tui/state.rs`)

1. **Add `last_user_activity_time: Option<Instant>` to `TabState`**
   - Updated by any keypress or mouse event on the active tab.
   - Used by Feature 1 to suppress stuck detection when the user is engaged.

2. **Add `yolo_countdown_started_at: Option<Instant>` to `TabState`**
   - Single authoritative timestamp for yolo countdown timing, covering both active-tab dialog and background tab-bar countdown.
   - Replaces the `started_at` field currently embedded in `Dialog::WorkflowYoloCountdown`.
   - Set by `tick_all()` when a tab (active or background) first becomes stuck in yolo mode.
   - Cleared when the tab stops being stuck (new output arrives) or when the countdown expires.

3. **Add `record_user_activity(&mut self)` method to `TabState`**
   - Sets `last_user_activity_time = Some(Instant::now())`.
   - Called in the event loop wherever `acknowledge_stuck()` is called for the active tab.
   - Distinct from `acknowledge_stuck()` so that semantics remain clear: `acknowledge_stuck` resets the output-based timer; `record_user_activity` records intent-to-suppress.

4. **Modify `is_stuck(&self, is_active: bool) -> bool`**
   - When `is_active = true`: return false if `last_user_activity_time.elapsed() < STUCK_TIMEOUT`, regardless of `last_output_time`. Only mark active tab as stuck when the user has also been idle for `STUCK_TIMEOUT`.
   - When `is_active = false`: current behavior — only check `last_output_time` vs `STUCK_TIMEOUT`.
   - Update all call sites (in `tick_all()` and `tab_color()`) to pass the correct `is_active` value.

### Feature 1: Active-Tab Stuck Suppression (`src/tui/input.rs`, `src/tui/mod.rs`, `src/tui/state.rs`)

5. **Update `handle_key()` in `input.rs`**
   - At line 112 (before dialog dispatch), call both `acknowledge_stuck()` and `record_user_activity()` on the active tab.

6. **Update mouse event handler in `mod.rs`**
   - When processing `Event::Mouse`, call `record_user_activity()` in addition to the existing `acknowledge_stuck()` call.

7. **Update `tick_all()` for active tab stuck detection**
   - Use `is_stuck(true)` for the active tab when deciding whether to open `Dialog::WorkflowControlBoard` or `Dialog::WorkflowYoloCountdown`.
   - When the active tab transitions from stuck to not-stuck (due to user activity), close any open `Dialog::WorkflowYoloCountdown` and clear `yolo_countdown_started_at` if the dialog was opened from active-tab logic.

### Feature 2: Background Yolo Countdown (`src/tui/state.rs`, `src/tui/mod.rs`, `src/tui/render.rs`, `src/tui/input.rs`)

8. **Unify countdown timing: refactor `Dialog::WorkflowYoloCountdown`**
   - Remove the `started_at` field from the dialog variant (or keep it but always populate it from `TabState.yolo_countdown_started_at`).
   - Dialog rendering reads `tab.yolo_countdown_started_at` for elapsed time and remaining seconds.

9. **Extend `tick_all()` to cover all tabs**
   - For each tab (active or background), when `yolo_mode = true` and `is_stuck(is_active) = true`:
     - Set `yolo_countdown_started_at = Some(Instant::now())` if not already set.
     - If `yolo_countdown_started_at.elapsed() >= YOLO_COUNTDOWN_DURATION`: set `yolo_countdown_expired = true`, clear `yolo_countdown_started_at`, close dialog if open, clear `workflow_stuck_dialog_opened`.
   - For each tab where the container is no longer stuck (new output received): clear `yolo_countdown_started_at` (countdown resets if container resumes and stalls again later).
   - For the active tab: if `yolo_countdown_started_at.is_some()` and no yolo dialog is open, open `Dialog::WorkflowYoloCountdown` as before.
   - For background tabs: do NOT open a dialog; rely on tab bar rendering instead.

10. **Update tab-switching actions in `mod.rs`**
    - When switching TO a tab with `yolo_countdown_started_at.is_some()` (and not yet expired): open `Dialog::WorkflowYoloCountdown` using the existing `yolo_countdown_started_at` (preserving remaining time).
    - When switching AWAY from a tab with `Dialog::WorkflowYoloCountdown` open: close the dialog (set `tab.dialog = None`). Do not clear `yolo_countdown_started_at`; countdown continues in background mode.

11. **Allow Ctrl+A / Ctrl+D while yolo dialog is open in `input.rs`**
    - In the `Dialog::WorkflowYoloCountdown` input handler (`handle_workflow_yolo_countdown()`), intercept Ctrl+A and Ctrl+D.
    - Return `Action::SwitchTabLeft` or `Action::SwitchTabRight` respectively.
    - The tab-switching action handler in `mod.rs` will close the dialog as part of step 10.

12. **Background tab color and label for yolo countdown in `state.rs` and `render.rs`**

    Add method `background_yolo_color(&self) -> Option<Color>` on `TabState`:
    - Returns `None` if `yolo_countdown_started_at` is None.
    - Otherwise, alternates each second: even-second seconds since start → `Color::Yellow`; odd seconds → `Color::Magenta` (purple).

    Add method `background_yolo_label(&self, tab_width: u16) -> Option<String>` on `TabState`:
    - Returns `None` if `yolo_countdown_started_at` is None.
    - Otherwise returns alternating `"⚠️  yolo in {secs_remaining}"` (yellow phase) or `"🤘 yolo in {secs_remaining}"` (purple phase), truncated to fit `tab_width`.

    Modify `tab_color()` to accept `is_active: bool`:
    - When `is_active = false`: check `background_yolo_color()` first and return it if `Some`.
    - Existing color logic (stuck → yellow, error → red, etc.) applies otherwise.

    Modify `tab_subcommand_label(tab_width, is_active: bool) -> String`:
    - When `is_active = false`: check `background_yolo_label()` first and return it if `Some`.

    Update `draw_tab_bar()` in `render.rs`:
    - Pass `is_active: bool` to `tab_color()` and `tab_subcommand_label()`.


## Edge Case Considerations

- **Countdown already expired when switching to tab**: If `yolo_countdown_started_at` has elapsed `>= YOLO_COUNTDOWN_DURATION` at the moment the user switches to that tab, `tick_all()` will have already set `yolo_countdown_expired = true` and cleared `yolo_countdown_started_at`. The tab switch finds no active countdown, so no dialog is opened and the tab is already normal. The workflow advances on the next `tick_all()` cycle that processes `yolo_countdown_expired`.

- **Multiple background yolo tabs simultaneously**: Each tab has its own `yolo_countdown_started_at`. They alternate colors independently. The tab bar may flash different colors for different tabs simultaneously; this is acceptable given the urgency of yolo mode.

- **Container resumes output mid-countdown (background tab)**: `tick_all()` should detect that `is_stuck(false)` is no longer true, clear `yolo_countdown_started_at`, and return the tab to its normal color. If the container stalls again, a fresh countdown begins.

- **User presses Esc while yolo dialog is open (active tab, existing backoff behavior)**: The dialog is closed and `workflow_stuck_dialog_dismissed_at` is set with 60s backoff. The `yolo_countdown_started_at` timer continues running. Because the countdown duration is 60s and the backoff is 60s, the countdown will typically expire before the dialog would reopen, resulting in auto-advance without further prompting. This is acceptable: Esc signals willingness to let the workflow advance.

- **User interacts actively with a tab that has an open yolo dialog**: `record_user_activity()` fires, `is_stuck(true)` returns false for the active tab. `tick_all()` should detect that the active tab is no longer stuck and close the open yolo dialog, clearing `yolo_countdown_started_at`. The tab returns to its normal state since the user is actively reading.

- **Switching away from the active tab while it has `last_user_activity_time` set recently**: The formerly-active tab is now a background tab. Its `last_user_activity_time` is irrelevant for background stuck detection because `is_stuck(false)` ignores it. If it was not stuck before the switch, the 10s background timer begins fresh from `last_output_time`.

- **`workflow_stuck_dialog_opened` flag interaction**: This flag prevents duplicate dialogs. With the new unified `yolo_countdown_started_at` as the authoritative timer, ensure that `workflow_stuck_dialog_opened` is still used consistently to gate dialog re-opening on the active tab (preventing `tick_all()` from repeatedly opening the dialog each frame).

- **Tab close while yolo countdown is running**: If a tab with `yolo_countdown_started_at` set is closed, `close_tab()` in `App` removes it. No cleanup needed for timers since `TabState` is dropped.


## Test Considerations

**Unit tests (in `src/tui/state.rs`):**
- `is_stuck(true)` returns false when `last_user_activity_time` is within `STUCK_TIMEOUT`, even if `last_output_time` is older than `STUCK_TIMEOUT`.
- `is_stuck(false)` returns true based only on `last_output_time`, ignoring `last_user_activity_time`.
- `record_user_activity()` sets `last_user_activity_time` and does not affect `last_output_time`.
- `background_yolo_color()` returns `Color::Yellow` for even elapsed seconds and `Color::Magenta` for odd, given a known `yolo_countdown_started_at`.
- `background_yolo_label()` returns properly formatted "yolo in {N}" text with correct countdown value and alternating emoji.
- `yolo_countdown_started_at` is set in `tick_all()` for a background stuck yolo tab and not reset until the countdown expires or the container resumes.
- `yolo_countdown_expired` is set and `yolo_countdown_started_at` is cleared when the countdown elapses for a background tab.

**Integration tests (`tests/`):**
- Active tab receives keypress while stuck → `is_stuck(true)` returns false → no yellow color change, no dialog opened.
- Active tab is idle for `STUCK_TIMEOUT` and user is also idle → dialog opens as before.
- Background tab enters stuck state in yolo mode → `yolo_countdown_started_at` is set → tab color alternates → label shows countdown.
- Background yolo tab countdown expires → `yolo_countdown_expired = true` → workflow advances without user action.
- Switching to background yolo tab with in-progress countdown → dialog opens with remaining time (not restarted from 60s).
- Yolo dialog open on active tab → Ctrl+D pressed → dialog closes → new active tab is focused → previous tab reverts to background countdown mode.
- Switching away from active tab with open yolo dialog → dialog closes on that tab → countdown continues in background mode.
- Container in background tab produces new output during yolo countdown → `yolo_countdown_started_at` cleared → tab returns to normal color.

**End-to-end tests:**
- Launch two-tab session with a yolo workflow; stall one tab in the background; verify tab bar alternation and auto-advance without switching to it.
- Open yolo dialog, switch tabs with Ctrl+A/Ctrl+D, switch back, verify dialog reflects remaining time and countdown was not restarted.


## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- All new `TabState` fields of type `Instant` or `Option<Instant>` should follow the existing patterns for `last_output_time`, `workflow_stuck_dialog_dismissed_at`, etc.
- `tick_all()` in `state.rs` is the canonical location for time-based state transitions; extend it rather than adding ticker logic elsewhere.
- The `Action` enum in `src/tui/mod.rs` is the established mechanism for tab-switching side effects; reuse `Action::SwitchTabLeft` and `Action::SwitchTabRight` from the yolo dialog input handler rather than calling tab-switch logic directly.
- The `draw_tab_bar()` function in `render.rs` is the single rendering site for all tab labels and colors; all visual changes for background yolo should be expressed through `tab_color(is_active)` and `tab_subcommand_label(width, is_active)` method signatures rather than ad-hoc logic in `draw_tab_bar()`.
- Ensure `yolo_countdown_started_at` is clearly documented as the single authoritative source of yolo countdown timing to prevent future drift from the `Dialog::WorkflowYoloCountdown` internal state.
