# Work Item: Feature

Title: Horizontal scrolling for wide Workflow Overviews
Issue: n/a

## Summary:
- The Workflow Overview (`src/frontend/tui/workflow_view.rs`) splits the full
  width of the overview area evenly across every topological column (stage):
  `base_col_w = (box_space / num_cols).max(4)`. Nothing stops a column from
  getting too narrow to read. A 14-column workflow in a normal-width terminal
  gets columns about 8–10 cells wide. After the rounded border, the status glyph
  and the `width - 6` name budget in `step_box_label_and_style`, only a few
  characters of each step name are left, and the agent/model title is cut to
  almost nothing. The overview is on screen but tells the user nothing.
- Add a **minimum usable column width**. When the columns would be narrower than
  that, the overview stops shrinking them. It shows as many full-width columns as
  fit and scrolls horizontally, one column at a time, through the rest.
- While horizontal scrolling is active, the overview shows edge markers for the
  hidden columns on either side. The status/hint bar advertises the scroll keys
  and the visible range, so the user can find the feature.
- **Keybinding:** Shift-← / Shift-→ scroll the overview one column left/right.
  The mouse gets horizontal-wheel and Shift+wheel support over the overview too.
  The requested Ctrl-[ / Ctrl-] pair cannot be used (see "Keybinding choice"
  below).

## User Stories

### User Story 1:
As a: user

I want to:
see step names and agent/model labels I can actually read in the Workflow
Overview, even when my workflow has many sequential stages (e.g. 14 columns)

So I can:
tell at a glance which step is running, which finished and which failed, without
guessing from 3-character truncations.

### User Story 2:
As a: user

I want to:
scroll the Workflow Overview left and right with a keyboard shortcut (and my
mouse) when it doesn't fit on screen, and see in the hint bar that I can

So I can:
look at earlier or later stages of a long workflow without resizing my terminal.

### User Story 3:
As a: user

I want to:
have the overview keep the running stage in view automatically unless I have
scrolled it myself

So I can:
leave a long workflow running and still see its progress without scrolling by
hand after every stage.


## Implementation Details:

### Trigger width and layout
- Add `pub const MIN_COLUMN_WIDTH: u16 = 18;` to `workflow_view.rs`, next to
  `STEP_BOX_HEIGHT`, with a doc comment. At 18 cells a box has 12 characters for
  the step name after `step_box_label_and_style`'s `width - 6` budget, and 16 for
  the agent/model title. That is enough for typical step names like
  `implement-api` and `claude/sonnet`. The value is a constant and is not
  user-configurable in this work item.
- Pull the horizontal layout math out of `render_workflow_overview` into a pure,
  unit-testable function:

  ```rust
  /// The horizontal slice of the overview's columns that fits in `width`.
  struct HorizontalLayout {
      first: usize,         // index of the first visible column (clamped offset)
      visible: usize,       // number of columns drawn
      col_w: u16,           // width of every visible column but the last
      hidden_left: usize,   // columns scrolled off to the left
      hidden_right: usize,  // columns scrolled off to the right
  }

  fn horizontal_layout(width: u16, num_cols: usize, offset: usize) -> HorizontalLayout
  ```

  - **Fits** (`num_cols` columns with their `num_cols - 1` arrow gaps give every
    column at least `MIN_COLUMN_WIDTH`): same result as today. `first = 0`,
    `visible = num_cols`, no hidden columns, `col_w` = the even split. The
    last column still takes the leftover width.
  - **Overflows:** reserve a 1-cell gutter on each side for the edge markers
    (below). Then `visible = max(1, (width - 2 + 1) / (MIN_COLUMN_WIDTH + 1))`,
    where the `+1` accounts for the arrow gap. Clamp `first` to
    `0..=num_cols - visible`. The visible columns share the remaining width
    evenly, so they are at least `MIN_COLUMN_WIDTH` wide and fill the area with
    no dead space on the right.
  - **Tiny terminal** (`width` too small for even one `MIN_COLUMN_WIDTH`
    column plus gutters): show one column at the full available width. This
    replaces the current `.max(4)` floor for this case, and it must never draw
    outside `area`.
- `render_workflow_overview` iterates over `columns[first..first + visible]`
  instead of every column. Everything else about how a column is drawn stays the
  same: vertical `scroll_offset`, `+ N more…` markers, setup/teardown titles,
  minimized `N steps…` summaries, arrows between visible columns. The loop's
  `col_idx + 1 == num_cols` checks become checks against the last *visible*
  column. The inter-column arrow after the last visible column is dropped when
  `hidden_right > 0`, because the right edge marker takes its place.
- Scrolling is **column-granular**, not cell-granular. A partially drawn
  column is never shown.
- `workflow_overview_height` does not change. Horizontal scrolling does not
  change how tall the overview is, because the tallest stage is still measured
  across *all* columns, so the height doesn't jump while scrolling.

### Edge markers
- When `hidden_left > 0`, draw `‹` in DarkGray in the left gutter, on the middle
  row of the first box row (the row the arrows use). When `hidden_right > 0`,
  draw `›` in the right gutter.
- If any **hidden** column contains a step in `error`/failed status, draw
  that side's marker red and bold, and if it is running, blue. Check error
  first, then running, then plain, using the same colour rules as
  `step_box_label_and_style`. A failure or live step that is scrolled out of view
  still gets noticed. Add a helper
  `fn hidden_status_hint(cols: &[Vec<&WorkflowStepView>]) -> &'static str` that
  reuses `stage_status`.

### State
- Add `workflow_overview_hscroll_offset: usize` and
  `workflow_overview_hscroll_follow: bool` (default `true`) to the tab state in
  `src/frontend/tui/tabs.rs`, next to `workflow_overview_scroll_offset`, and
  initialise them where that field is initialised. The offset is per tab, like
  the vertical offset.
- Add `last_overview_hlayout: Option<HorizontalLayout>` (or just
  `hidden_left`/`hidden_right`/`first`/`visible`/`num_cols`), written by
  `render.rs` next to `last_overview_rect`. The key handler, mouse handler and
  status bar can then tell whether scrolling is active and what range is visible
  without redoing the layout. Clear it wherever `last_overview_rect` is cleared
  (`render.rs`, `app.rs`).
- `render_workflow_overview` takes the horizontal offset as a new parameter and
  returns the resolved `HorizontalLayout`. The renderer does the clamping, so
  the stored offset can never leave the overview blank after a resize or after a
  dynamic workflow changes shape. `render.rs` writes the clamped `first` back to
  the tab.

### Follow-the-running-stage
- While `workflow_overview_hscroll_follow` is `true`, `render.rs` picks the
  offset each frame so that the column holding `state.current_step` is visible.
  Change the offset as little as possible: keep it if the column is already
  visible, otherwise scroll just far enough, so the current column ends up last
  in view when moving right. When no step is current, keep the offset where it
  is.
- Any manual horizontal scroll (key or mouse) sets `follow = false`. Scrolling
  all the way right (`hidden_right == 0`) sets `follow = true` again, like a
  log view that re-attaches when scrolled to the bottom. A new workflow starting
  on the tab resets `follow = true` and the offset to 0.
- Ctrl-O (`Action::ToggleWorkflowOverview`) already resets the vertical offset.
  It leaves the horizontal offset/follow state alone, because
  minimizing/maximizing doesn't change the column count.

### Keybinding choice
- **Ctrl-[ is not viable.** Terminals send Ctrl-[ as the ESC byte (0x1b), which
  cannot be told apart from the Esc key without the kitty keyboard protocol.
  Binding it would break Esc everywhere: dismissing dialogs, deselecting the
  execution window, and Esc forwarded to agents in a maximized container. It
  would also make the terminal wait on escape-sequence timeouts. Ctrl-] alone
  works (legacy decoders report it as Ctrl+'5', just as Ctrl-\ arrives as
  Ctrl+'4'; see `keymap.rs`), but it has no usable left-hand partner.
- **Use Shift-← / Shift-→.** Every mainstream terminal reports these distinctly
  (CSI `1;2D` / `1;2C`), they point the way they scroll, and nothing in awman
  uses them today. The command box binds only plain and Ctrl arrows.
- Add `Action::ScrollWorkflowOverviewLeft` and
  `Action::ScrollWorkflowOverviewRight` to `keymap.rs` with doc comments in the
  same style as `ToggleWorkflowOverview`.
- `map_key` has no app state, so it can't know whether the overview currently
  overflows. Map Shift-←/→ to the new actions in the global block
  **only when a new `overview_hscroll_active: bool` input says so**. Either add a
  parameter to `map_key`, or add a thin wrapper that the event loop calls with
  `tab.last_overview_hlayout.is_some_and(|l| l.hidden_left + l.hidden_right > 0)`.
  Pick whichever touches fewer call sites. The rules:
  - Active in `CommandBox`, `ExecutionWindow` and `ContainerMaximized` (before
    the `ForwardToPty` fall-through, like Ctrl-O), so scrolling is always
    available while the overview is on screen.
  - Not active in `Dialog` or `SquadList`. The squad grid already uses
    plain ←/→, and dialogs own their keys.
  - When the overview fits (no overflow), Shift-←/→ behave exactly as they do
    today (ignored in the command box, forwarded to the PTY when maximized).
    Agents lose Shift-arrows only while a too-wide overview is on screen, and
    this should be written down in a comment next to the binding.
- In `key_handler.rs`, the new actions change the offset by ±1 (saturating;
  the renderer clamps the upper bound), set `follow` as described above, and
  clear `mouse_selection` only if the change moves anything the selection is
  anchored to. The overview is not selectable, so normally leave it alone.

### Mouse
- In `mouse_handler.rs`'s overview hit-test, handle
  `MouseEventKind::ScrollLeft` / `ScrollRight` (horizontal wheel / trackpad
  swipe) and `ScrollUp`/`ScrollDown` **with `KeyModifiers::SHIFT`** as
  horizontal scroll, and only when horizontal scrolling is active. Plain
  vertical wheel keeps scrolling the stage vertically, as it does today.

### Hint bar
- In `render/status_bar.rs`, next to the existing
  `ctrl-o minimize/maximize workflow overview` span: when a workflow is active
  **and** the last layout overflowed, push
  `" · shift-←/→ scroll stages (3–8 of 14) "`, showing the 1-based first–last
  visible column indices and the total, in DarkGray.
- Put the scroll hint **before** the Ctrl-O hint. The bar is one row and gets
  cut off on the right, and the scroll hint only exists when the user needs it.
  Include it in every running-state branch that already shows the Ctrl-O hint,
  including the `ContainerMaximized` branch.
- Put the text in a small `pub(crate) fn overview_scroll_hint(first, visible,
  total) -> String`, in the style of `squad_failure_text`, so it can be unit
  tested.


## Edge Case Considerations:
- **Exactly at the threshold:** a layout where every column comes out at exactly
  `MIN_COLUMN_WIDTH` fits and must not scroll or show markers. Test the
  boundary on both sides.
- **Terminal resize** while scrolled: the renderer clamps the offset. Growing
  the terminal until everything fits removes the markers and the hint, and the
  Shift-arrows go back to being passed through.
- **Dynamic workflows / resume** that change the number of columns
  mid-run: clamping keeps the view non-empty. Follow mode keeps the current
  stage visible.
- **Git sidebar open** (Ctrl-G): the overview gets narrower, which can
  turn scrolling on. The layout uses the actual `chunks[3]` width, so this needs
  no special handling. Test it anyway.
- **Setup/teardown columns** are scrolled like any other column. With follow
  mode on, setup stays visible while setup runs.
- **Minimized overview:** horizontal scrolling works the same way. Each column
  is a single `N steps…` box.
- **Parallel `max_concurrent` queued steps** and the vertical `+ N more…` marker
  still work in every visible column, with the vertical offset shared as
  before.
- **Width below one column + gutters:** one column at full width, no gutters if
  they don't fit, and never a panic from `u16` underflow (use saturating
  arithmetic throughout).
- **Tab switching:** offset and follow state are per tab. Another tab's
  workflow is never scrolled by keys pressed on this tab.
- **Squad attach** sessions render the same overview through `render.rs`, so
  they get horizontal scrolling automatically. The squad *list* view (card
  grid) is not affected.
- **Headless/command mode** has no overview and is not affected.

## Test Considerations:
- Unit tests (`workflow_view.rs` `mod tests`):
  - `horizontal_layout` when everything fits: offset ignored, all columns visible,
    `col_w` matches today's even split.
  - Overflow at a few widths (e.g. 80, 120, 200 cells × 14 columns): correct
    `visible`, `col_w >= MIN_COLUMN_WIDTH`, total drawn width ≤ `width`.
  - Offset clamping: past the end → `first = num_cols - visible`; 0 → no left
    hidden columns.
  - Exact-threshold boundary (fits at `N`, overflows at `N - 1`).
  - Tiny width: one column, no underflow.
  - `hidden_status_hint` picks error over running over plain.
- Render tests (`tests/render_tests.rs`, `TestBackend`):
  - A 14-column workflow at 100 cells shows full step names, the `›` marker,
    no `‹` marker at offset 0, and both markers mid-scroll.
  - A failed step in a hidden column turns its side's marker red.
  - A workflow that fits renders exactly as before (regression guard for
    existing snapshots/assertions).
  - The hint bar shows `shift-←/→ scroll stages (1–5 of 14)` when overflowing,
    and doesn't show it when the overview fits.
- Keymap tests (`tests/key_handler_tests.rs` / `keymap.rs` tests):
  - Shift-←/→ map to the new actions when overflow is active in `CommandBox`,
    `ExecutionWindow`, `ContainerMaximized`.
  - They fall through to existing behaviour (incl. `ForwardToPty`) when
    overflow is inactive, and in `Dialog`/`SquadList`.
- Key handler / follow-mode tests: manual scroll disables follow; scrolling to
  the right end re-enables it; a current step outside the visible range moves the
  offset only when follow is on.
- Mouse tests: horizontal wheel and Shift+wheel over the overview scroll it
  horizontally only when overflowing. Plain wheel still scrolls vertically.
- End-to-end: a fixture workflow TOML with ≥14 sequential steps runs in a narrow
  `TestBackend` without panicking and without drawing outside the overview rect.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Every shortcut is defined in `keymap.rs` (its module doc says so). Add no
  key-matching logic to `key_handler.rs` or `mouse_handler.rs` beyond routing
  actions/events.
- Keep layout math pure and separate from `Frame` rendering so it can be
  unit tested, matching `workflow_overview_height` / `build_workflow_columns`.
- Use saturating `u16` arithmetic, as the surrounding render code does.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- `docs/02-using-the-tui.md`: in "The Workflow Overview — Ctrl-O" section,
  explain that a wide workflow scrolls horizontally, what the `‹`/`›` markers
  (and their colours) mean, that the running stage stays in view until the user
  scrolls by hand, and the hint-bar text. Add **Shift+←/→** (and horizontal
  wheel / Shift+wheel) to each keyboard-shortcut table that lists Ctrl+O (around
  lines 737, 779, 802), noting that it is only active while the overview
  overflows.
- `docs/05-workflows.md`: next to the existing sentence about the mouse wheel
  and `+ N more…` (around line 1181), add a short note on horizontal scrolling
  for workflows with many stages.
- **Never create work-item-specific docs** (e.g., no "WI 0118 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
