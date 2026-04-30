# Work Item: Task

Title: Bounded Output Buffer (Ring Buffer for output_lines)
Issue: issuelink

## Summary:

Replace the unbounded `Vec<String>` output buffer in `TabState` with a bounded `VecDeque<String>` that evicts the oldest lines when a configurable maximum is reached. Also maintain a running `total_visual_rows` counter to eliminate the O(n) per-frame scroll calculation in `draw_exec_window`. This prevents unbounded memory growth during long sessions and removes a significant per-frame CPU cost.

## User Stories

### User Story 1:
As a: user

I want to:
run amux for hours with high-output containers without memory growing unboundedly

So I can:
leave agent tabs running overnight without risking OOM or system slowdown

### User Story 2:
As a: user

I want to:
see consistent TUI frame rates regardless of how much output a container has produced

So I can:
work smoothly even after a tab has accumulated thousands of log lines

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Areas 1.2 and 2.1).

### Current Behaviour:

- `output_lines: Vec<String>` in `TabState` (state.rs:301) — unbounded, grows forever.
- `draw_exec_window` in `render.rs:225–235` iterates **all** `output_lines` each frame to compute `total_visual` rows for scroll offset calculation — O(n) where n = all lines ever received.

### Proposed Changes:

#### 1. Replace `Vec<String>` with bounded `VecDeque<String>`

```rust
// state.rs
pub output_lines: VecDeque<String>,
pub output_lines_max: usize,  // configurable, default 10_000
```

When pushing a new line, if `output_lines.len() >= output_lines_max`, call `output_lines.pop_front()` first.

#### 2. Add a running visual row counter

```rust
pub total_visual_rows: usize,
```

Update this counter whenever lines are pushed or popped, using the same `line.width() / inner_width` formula currently computed per-frame. This requires knowing `inner_width` at push time, which is problematic since it depends on terminal size.

**Alternative (simpler):** Keep the O(n) calculation but cap n at `output_lines_max` (10,000). This bounds the per-frame cost to a fixed maximum regardless of session length. The running counter optimisation can be deferred to a follow-on item.

#### 3. Update all push/clear sites

All callers of `output_lines.push()`, `output_lines.clear()`, and `output_lines.last_mut()` must be updated to use `VecDeque` API:
- `push` → `push_back`
- `last_mut` → `back_mut`
- Iterator usage in `render.rs` — `VecDeque::iter()` is the same API as `Vec::iter()`

#### 4. Scroll offset validity

When lines are evicted from the front of the deque, `scroll_offset` (which counts from the bottom) remains valid without adjustment — scroll offset is relative to the bottom of the buffer, not absolute line indices.

### Configuration:
- The default maximum (10,000 lines) should be a named constant in `state.rs`.
- Consider exposing it as a future config option in `~/.amux/config.json`.

## Edge Case Considerations:
- **CLEAR_MARKER**: `status --watch` sends this to clear the buffer. With `VecDeque`, `clear()` still works correctly.
- **Live line updates** (`pty_live_line`): the last entry in the deque is updated in-place on spinner/progress lines. The eviction check must not evict the live line.
- **scroll_offset**: verify that scroll_offset semantics remain correct when lines are evicted from the front. Since scroll_offset is "lines from bottom", eviction from the front does not invalidate it.
- **Empty buffer**: ensure push-then-evict handles the case where max is 0 (forbid or set minimum of 100).

## Test Considerations:
- Unit test: push `output_lines_max + 1` lines, assert `output_lines.len() == output_lines_max`.
- Unit test: push lines, evict some, assert scroll_offset remains within valid range.
- Memory test: open a tab, send 100,000 lines via `output_tx`, assert `output_lines.len() <= output_lines_max`.
- Render test: confirm `draw_exec_window` produces correct output with a VecDeque buffer.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Primary files: `src/tui/state.rs`, `src/tui/render.rs`.
- Use `use std::collections::VecDeque` — already in std, no new dependency.
