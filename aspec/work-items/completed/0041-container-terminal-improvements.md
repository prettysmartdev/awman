# Work Item: Enhancement

Title: Container Terminal Improvements
Issue: issuelink

## Summary:
Two improvements to the container terminal window in the TUI: (1) make terminal text selectable and copyable via mouse drag and Cmd/Ctrl+C, and (2) increase usable scrollback history so users can review more than the current ~50 visible lines of output.

## User Stories

### User Story 1:
As a: user

I want to: select text in the container terminal with my mouse and copy it to the clipboard

So I can: paste agent output, error messages, or command results into other tools without leaving the TUI

### User Story 2:
As a: user

I want to: scroll back through significantly more terminal output history than currently available

So I can: review long-running command output, build logs, or test results that extend well beyond the visible terminal area

### User Story 3:
As a: user

I want to: see a clear visual indicator of my position within the full scrollback history

So I can: understand how far back I've scrolled relative to the total available output

## Implementation Details:

### Feature 1: Text Selection and Copy

**Feasibility:** Implementable using the existing `vt100` cell-by-cell rendering infrastructure.

**Approach:**
- Track mouse drag events (`MouseEventKind::Down`, `MouseEventKind::Drag`, `MouseEventKind::Up`) in `src/tui/mod.rs`, limited to when the container window is `Maximized`
- Store selection state in `TabState`: anchor cell `(row, col)` on `MouseDown`, extend to current cell on `Drag`, finalize on `MouseUp`
- Convert mouse pixel/character coordinates to vt100 cell coordinates by accounting for the container window's screen offset (computed in `draw_container_window()` in `src/tui/render.rs`)
- Highlight selected cells during rendering in `render_vt100_screen()` / `render_vt100_screen_no_cursor()` by applying an inverted or highlighted `Style` to cells within the selection range
- Audit all existing keybindings in `src/tui/input.rs` before assigning a copy key. If `Ctrl+C` is already bound (e.g., as quit/cancel), do not reassign it — use `Ctrl+Y` or another unoccupied binding instead. The copy keybinding must not conflict with any existing TUI action; this audit is a hard prerequisite, not an afterthought
- On the chosen copy key (e.g., `Ctrl+C` if unoccupied, otherwise `Ctrl+Y`), extract cell contents from the selection range using `screen.cell(row, col).contents()` and write to the system clipboard using the `arboard` crate (cross-platform, no system deps when statically linking)
- Selection should account for scroll offset so that text in the scrollback area is selectable, not just the live screen
- Clear selection on `Esc`, window minimize, or new output that changes the layout

**New dependency:** `arboard = "3"` — pure Rust clipboard access for macOS, Linux (X11/Wayland), and Windows; compatible with static linking requirements

**State additions to `TabState` (`src/tui/state.rs`):**
```
terminal_selection_start: Option<(u16, u16)>,  // (row, col) in vt100 space
terminal_selection_end: Option<(u16, u16)>,
terminal_selection_snapshot: Option<Vec<Vec<String>>>,  // per-cell contents captured at MouseDown; used for drag/copy to prevent live-output coordinate drift
```

### Feature 2: Scrollback History Expansion

**Root cause of current ~50 line limit:**
The `vt100::Parser` is created with a 1,000-line scrollback buffer (`src/tui/state.rs`, `Parser::new(rows, cols, 1000)`), but the scroll offset cap in the mouse handler (`src/tui/mod.rs`) uses `screen().size().0` (screen height, ~50 rows) as the maximum — not the actual scrollback depth. This means users can only scroll back one screen height worth of content regardless of how much is stored.

**Fix:**
- Replace the `max_scroll` calculation with the actual number of scrollback rows available from the parser. The `vt100::Screen` exposes `scrollback()` — use its length to determine the true upper bound
- Verify that `render_vt100_screen_no_cursor()` correctly renders from the scrollback buffer at arbitrary offsets; if it currently only renders from `screen()`, it needs to composite visible rows from the scrollback rows at the given offset
- Increase the parser scrollback size from 1,000 to a larger configurable value (e.g., 10,000 lines default) to support long-running builds and test runs
- Add a configurable `terminal_scrollback_lines` option in the per-repo config (`aspec/.amux.json`) and global config (`~/.amux/config.json`) so power users can tune memory usage
- Update the scrollback position indicator in `render.rs` (currently shows "↑ scrollback (N lines up)") to also show total available lines, e.g., "↑ scrollback (N / M lines)"

**Keyboard scroll speed:** Consider increasing lines-per-keypress from the current value to 5–10 lines for large scrollback buffers.

## Edge Case Considerations:

- **Terminal resize during selection:** When the terminal is resized, vt100 re-wraps lines and cell coordinates shift. Clear any active selection on resize events to prevent stale coordinate mapping
- **Selection across wrapped lines:** Multi-line selection should copy with newlines at logical line boundaries, not at every screen-wrap boundary — evaluate whether vt100's cell API exposes line-wrap metadata; if not, use a heuristic (trailing non-space cells → no newline)
- **Copy with ANSI escape sequences:** Strip all ANSI/color attributes from copied text; clipboard content should be plain text only
- **Scrollback memory pressure:** A 10,000-line buffer at 220 columns × 10k rows is ~2MB per tab. With many tabs open, this can add up. Default should balance usability vs. memory. If the user configures a very large buffer, warn at startup if total estimated memory exceeds a threshold
- **Clipboard unavailable:** On headless Linux environments (no X11/Wayland), `arboard` will fail to initialize. Degrade gracefully — log a warning, disable the copy feature, and show a one-time status bar notice rather than panicking
- **Mouse capture interaction:** Mouse events are currently captured globally by the TUI. Ensure that mouse-down inside the container terminal area is distinguished from mouse-down on the outer workflow/tab UI to prevent unintended selection triggering layout changes
- **Zero-length selection:** A click without drag should not produce an empty copy operation; treat it as a cursor position acknowledgment only
- **Scrollback at live tail:** When `container_scroll_offset == 0` (live view), the container may still be receiving output. On `MouseDown`, snapshot the current `vt100::Screen` state (or at minimum the rendered cell contents for the visible area) and use that snapshot for all subsequent `Drag` and `Up` coordinate resolution and text extraction. This snapshot is mandatory — without it, new output arriving mid-drag will shift cell coordinates under the selection, causing the highlight to wander over the wrong text and the copied content to be incorrect. Do not defer this to a "nice to have"

## Test Considerations:

- **Unit tests for selection coordinate mapping:** Given a known container window layout and scroll offset, verify that a mouse position `(x, y)` maps to the correct `(vt100_row, vt100_col)` — including when scrolled into the scrollback buffer
- **Unit tests for selection text extraction:** Given a `vt100::Screen` with known content, verify that extracting text from a selection range produces the expected plain-text string with correct newline handling
- **Unit tests for scrollback cap fix:** Verify that `container_scroll_offset` can reach beyond one screen height when the scrollback buffer contains more data; verify the upper bound matches actual scrollback depth
- **Unit tests for config parsing:** Verify `terminal_scrollback_lines` is read from both global and per-repo config, with correct precedence and a fallback default
- **Integration tests for clipboard:** Mock the clipboard interface (via trait abstraction) to test the copy flow without requiring a real display server in CI
- **Integration tests for resize-clears-selection:** Simulate a terminal resize event while a selection is active and verify selection state is cleared
- **Memory bounds test:** Extend the existing `tests/memory_bounds.rs` to verify that the new scrollback default (10,000 lines) doesn't exceed acceptable memory thresholds per tab
- **End-to-end test for scrollback display:** Drive the TUI with simulated output exceeding one screen height, scroll up, and verify the scrollback position indicator reflects the correct line count

## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`
- **Key files to modify:**
  - `src/tui/state.rs` — add selection state fields to `TabState`; fix scrollback cap; update `start_container()` to use configurable scrollback size
  - `src/tui/mod.rs` — add `MouseDown`/`Drag`/`Up` event handling; add `Ctrl+C`/`Cmd+C` copy handler
  - `src/tui/render.rs` — highlight selected cells in `render_vt100_screen()` and `render_vt100_screen_no_cursor()`; update scrollback indicator
  - `src/tui/input.rs` — wire copy keybinding; optionally increase keyboard scroll speed
  - `src/config.rs` (or equivalent config module) — add `terminal_scrollback_lines` field
  - `Cargo.toml` — add `arboard = "3"` dependency
- Abstract the clipboard write behind a trait so it can be mocked in tests (consistent with amux's preference for testable, modular code)
- The `vt100` crate's `Screen::cell(row, col)` and `Screen::scrollback()` are the primary APIs for both features — review the `vt100` 0.15 docs/source to confirm the scrollback access API before implementing
- Ensure the `arboard` crate is compatible with static linking; verify on macOS, Linux, and Windows before merging
