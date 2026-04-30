# Work Item: Task

Title: Event-Driven Render Loop (Dirty Flag)
Issue: issuelink

## Summary:

Add a `needs_render` dirty flag to `App` so that `terminal.draw()` is skipped when no state has changed since the last frame. Currently the render loop unconditionally rebuilds the entire Ratatui widget tree and calls `terminal.draw()` every 16 ms (~60 Hz) regardless of whether anything changed, burning CPU while the user is idle.

## User Stories

### User Story 1:
As a: user

I want to:
have amux consume near-zero CPU when idle (no running container, no input)

So I can:
run amux alongside resource-intensive workloads (builds, tests, other containers) without CPU contention

### User Story 2:
As a: user

I want to:
see the TUI remain responsive at high tab counts without frame budget overruns

So I can:
monitor 10–20 concurrent agent tabs without visible lag

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Area 1.1).

### Current Behaviour (`src/tui/mod.rs:104–112`):

```rust
loop {
    terminal.draw(|f| render::draw(f, &mut app))?;  // always redraws
    if event::poll(Duration::from_millis(16))? {
        // handle event
    }
    tick_all();
}
```

### Proposed Change:

1. Add `needs_render: bool` field to `App` (default `true`).
2. In `tick_all()`, set `app.needs_render = true` if any tab's `tick()` drained at least one message (PTY data, text output, channel signal).
3. In the input handling path, set `app.needs_render = true` on every key/mouse event.
4. In the event loop, skip `terminal.draw()` when `needs_render` is `false`.
5. After drawing, reset `needs_render = false`.
6. Keep `needs_full_redraw` for the suspend/restore path (forces `terminal.clear()` before next draw).

### Key considerations:
- `tick_all()` must return or set a flag indicating whether any state changed, so the event loop knows whether to render.
- Dialogs, phase transitions, and workflow state changes must all set `needs_render = true`.
- The 16 ms `event::poll` timeout ensures the loop still wakes up regularly even without events (e.g., for stuck-tab detection).

## Edge Case Considerations:
- A tab transitioning to Stuck state (via `is_stuck()` in `tick_all()`) must trigger a render even if no PTY data arrived.
- The container summary overlay appearing after container exit must trigger a render.
- Any `App` state change that affects visible output must be tracked.

## Test Considerations:
- Unit test: create an `App` with no running command, call `tick_all()`, confirm `needs_render` is `false`.
- Unit test: send a message via `output_tx`, call `tick_all()`, confirm `needs_render` is `true`.
- Integration test: measure that the event loop does not call `draw()` more than once per unique state change during an idle session.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Primary files: `src/tui/mod.rs`, `src/tui/state.rs`.
- The `needs_full_redraw` field already exists on `App` and serves as a model for this approach.
