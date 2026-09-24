//! Workflow Overview — horizontal display of workflow step progression.
//!
//! Layout:
//! - Agent steps are grouped into **topological columns** ("stages") by
//!   sorted `depends_on` signature (steps that share the same dependencies
//!   sit in the same column). Setup and teardown steps are never part of
//!   that DAG — they get their own dedicated leading/trailing column instead.
//! - Each step renders as a **3-row rounded box** with a status glyph, the
//!   step name, and a top-border title: the resolved `agent/model` for an
//!   agent step, or `[setup]`/`[teardown]` for a setup/teardown step.
//! - **Inter-column `→` arrows** sit on the middle row of the first row of
//!   boxes, joining adjacent columns.
//!
//! The overview has two display modes ([`WorkflowOverviewState`], toggled with
//! `Ctrl-O` — independently of the container PTY's own `Ctrl-M` min/max):
//! - **Minimized** (default) — one box per stage, 3 rows total. A
//!   single-step stage draws that step's normal box; a parallel stage draws
//!   a `N steps…` summary in the stage's aggregate status colour.
//! - **Maximized** — every step of every stage gets its own box, stacked
//!   vertically at the same indent. Nothing is ever rolled up: completed
//!   parallel siblings keep their own box, name, agent label, and colour.
//!   The overview grows into the vertical space the frame can spare between
//!   the tab bar and the command box (the caller's `max_height`, which the
//!   renderer halves while a maximized container PTY is also on screen); when
//!   even that is not enough, the last visible row becomes a `+ N more…`
//!   overflow box and the mouse wheel scrolls the stage.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::data::workflow_state::WorkflowState;
use crate::frontend::tui::tabs::{
    StepViewStatus, WorkflowOverviewState, WorkflowStepKind, WorkflowStepView, WorkflowViewState,
};

/// Rows occupied by one step box (rounded border + one content row).
pub const STEP_BOX_HEIGHT: u16 = 3;

/// Narrowest a Workflow Overview column is allowed to get before the overview
/// stops shrinking columns and scrolls horizontally instead.
///
/// At 18 cells a box keeps 12 characters for the step name (after
/// [`step_box_label_and_style`]'s `width - 6` budget) and 16 for the
/// agent/model title — enough for names like `implement-api` and labels like
/// `claude/sonnet`.
pub const MIN_COLUMN_WIDTH: u16 = 18;

/// The horizontal slice of the overview's columns that fits in a given width.
///
/// Produced by [`horizontal_layout`] and returned by
/// [`render_workflow_overview`], so callers can tell whether horizontal
/// scrolling is active (`hidden_left + hidden_right > 0`) and which columns
/// are on screen without redoing the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HorizontalLayout {
    /// Index of the first visible column (the offset, clamped).
    pub first: usize,
    /// Number of columns drawn.
    pub visible: usize,
    /// Width of every visible column but the last (which takes the leftover).
    pub col_w: u16,
    /// Columns scrolled off to the left.
    pub hidden_left: usize,
    /// Columns scrolled off to the right.
    pub hidden_right: usize,
}

impl HorizontalLayout {
    /// Whether some columns are scrolled out of view.
    pub fn overflows(&self) -> bool {
        self.hidden_left + self.hidden_right > 0
    }
}

/// Lay out `num_cols` overview columns across `width` cells, starting at
/// column `offset`.
///
/// - **Fits** — every column gets at least [`MIN_COLUMN_WIDTH`] after the
///   `num_cols - 1` one-cell arrow gaps: all columns are shown, `offset` is
///   ignored, and `col_w` is the even split (the last column takes the
///   leftover).
/// - **Overflows** — a 1-cell gutter is reserved on each side for the `‹`/`›`
///   edge markers, and as many columns of at least `MIN_COLUMN_WIDTH` as fit
///   (with their arrow gaps) share the remaining width. `offset` is clamped to
///   `0..=num_cols - visible`.
/// - **Tiny terminal** — `width` cannot hold one `MIN_COLUMN_WIDTH` column
///   plus both gutters: one column at the full width, no gutters.
///
/// Scrolling is column-granular: a partially drawn column is never shown.
pub fn horizontal_layout(width: u16, num_cols: usize, offset: usize) -> HorizontalLayout {
    // An empty area has nothing visible, like an empty workflow.
    if num_cols == 0 || width == 0 {
        return HorizontalLayout {
            first: 0,
            visible: 0,
            col_w: 0,
            hidden_left: 0,
            hidden_right: 0,
        };
    }

    // Fits: the even split (after the arrow gaps) is at least the minimum.
    // More than `u16::MAX` columns can never fit in a `u16` width.
    if let Ok(n) = u16::try_from(num_cols) {
        let even = width.saturating_sub(n - 1) / n;
        if even >= MIN_COLUMN_WIDTH {
            return HorizontalLayout {
                first: 0,
                visible: num_cols,
                col_w: even,
                hidden_left: 0,
                hidden_right: 0,
            };
        }
    }

    // Overflows. Tiny terminal: one full-width column, no gutters.
    let (visible, col_w) = if width < MIN_COLUMN_WIDTH.saturating_add(2) {
        (1usize, width)
    } else {
        let inner = width - 2;
        // `+ 1` because the last visible column needs no arrow gap after it.
        let fit = (inner as usize + 1) / (MIN_COLUMN_WIDTH as usize + 1);
        let visible = fit.clamp(1, num_cols);
        let gaps = (visible - 1) as u16;
        (visible, inner.saturating_sub(gaps) / visible as u16)
    };
    let first = offset.min(num_cols - visible);
    HorizontalLayout {
        first,
        visible,
        col_w,
        hidden_left: first,
        hidden_right: num_cols - visible - first,
    }
}

/// Width of the edge-marker gutter on each side of the overview: 1 while
/// columns are scrolled out of view and the width can spare it, else 0.
fn overview_gutter(layout: &HorizontalLayout, width: u16) -> u16 {
    if layout.overflows() && width >= MIN_COLUMN_WIDTH.saturating_add(2) {
        1
    } else {
        0
    }
}

/// The column follow mode keeps in view: a running setup/teardown phase,
/// otherwise the one holding `state.current_step`, else the first column with
/// a running step.
///
/// Only agent steps are matched by name, since setup/teardown steps never set
/// `current_step` and may share a name with an agent step. The running-step
/// fallback covers setup/teardown phases, which run with no current step.
pub(crate) fn follow_column(
    state: &WorkflowViewState,
    columns: &[Vec<&WorkflowStepView>],
) -> Option<usize> {
    // Phase progress may arrive while a resumed snapshot still names the last
    // agent as current. The running phase is the stage the user needs to see.
    columns
        .iter()
        .position(|column| {
            column
                .iter()
                .any(|s| s.kind != WorkflowStepKind::Agent && s.status == StepViewStatus::Running)
        })
        .or_else(|| {
            state.current_step.as_ref().and_then(|name| {
                columns.iter().position(|column| {
                    column
                        .iter()
                        .any(|s| s.kind == WorkflowStepKind::Agent && &s.name == name)
                })
            })
        })
        .or_else(|| {
            columns
                .iter()
                .position(|column| column.iter().any(|s| s.status == StepViewStatus::Running))
        })
}

/// The horizontal offset follow mode requests so that `column` is visible,
/// moving as little as possible: unchanged if it is already in view, else
/// just far enough (so a column to the right ends up last in view). `None`
/// keeps `offset`. The renderer still clamps the result.
pub(crate) fn follow_hscroll_offset(
    width: u16,
    num_cols: usize,
    offset: usize,
    column: Option<usize>,
) -> usize {
    let Some(column) = column else {
        return offset;
    };
    let layout = horizontal_layout(width, num_cols, offset);
    if layout.visible == 0 {
        offset
    } else if column < layout.first {
        column
    } else if column >= layout.first + layout.visible {
        column + 1 - layout.visible
    } else {
        layout.first
    }
}

/// Whether a manually scrolled overview re-attaches follow mode after
/// rendering `layout`: when everything fits, or when the view is scrolled all
/// the way right and the followed `column` is in view. Re-attaching while the
/// followed column is hidden would make follow snap the view straight back
/// on the next frame. A degenerate frame (nothing visible) never re-attaches.
pub(crate) fn hscroll_follow_reattaches(layout: &HorizontalLayout, column: Option<usize>) -> bool {
    if layout.visible == 0 {
        return false;
    }
    !layout.overflows()
        || (layout.hidden_right == 0
            && column.is_some_and(|c| c >= layout.first && c < layout.first + layout.visible))
}

/// Summarise the steps in scrolled-out columns for colouring an edge marker:
/// `"error"` if any failed, else `"running"` if any is running, else
/// `"plain"`. Error is checked first, then running, matching the colours of
/// [`step_box_label_and_style`].
fn hidden_status_hint(cols: &[Vec<&WorkflowStepView>]) -> &'static str {
    let steps: Vec<&WorkflowStepView> = cols.iter().flatten().copied().collect();
    match stage_status(&steps) {
        StepViewStatus::Error => "error",
        StepViewStatus::Running => "running",
        // A remediation outranks running in `stage_status`, but a running
        // step elsewhere should still tint the marker.
        StepViewStatus::Fixing if steps.iter().any(|s| s.status == StepViewStatus::Running) => {
            "running"
        }
        _ => "plain",
    }
}

/// Style for a `‹`/`›` edge marker given [`hidden_status_hint`]'s summary.
fn edge_marker_style(hint: &str) -> Style {
    match hint {
        "error" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "running" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// Compute the rows the Workflow Overview wants, clamped to `max_height`.
///
/// - Minimized → one box row (3 rows), whatever the shape of the workflow.
/// - Maximized → one box row per step in the widest stage, with **no** cap
///   other than `max_height` (the share of the body the caller is willing to
///   give up). The result is always a whole number of box rows, so a box is
///   never clipped mid-border.
///
/// Returns 0 when `state` has no steps, or when `max_height` cannot fit even
/// a single box.
pub fn workflow_overview_height(
    state: &WorkflowViewState,
    overview_state: WorkflowOverviewState,
    max_height: u16,
) -> u16 {
    if state.steps.is_empty() {
        return 0;
    }
    let rows = match overview_state {
        WorkflowOverviewState::Minimized => 1u16,
        WorkflowOverviewState::Maximized => {
            let columns = build_workflow_columns(state);
            columns.iter().map(|c| c.len()).max().unwrap_or(1).max(1) as u16
        }
    };
    let desired = rows.saturating_mul(STEP_BOX_HEIGHT);
    let cap = (max_height / STEP_BOX_HEIGHT) * STEP_BOX_HEIGHT;
    desired.min(cap)
}

/// Render the Workflow Overview into the given area.
///
/// `hscroll_offset` is the requested first visible column. It is clamped by
/// [`horizontal_layout`], and the resolved layout is returned so the caller
/// can store the clamped offset and tell whether horizontal scrolling is
/// active. Nothing is drawn, and an empty layout (`visible == 0`) is
/// returned, when the area or the workflow is empty.
pub fn render_workflow_overview(
    state: &WorkflowViewState,
    area: Rect,
    frame: &mut Frame,
    scroll_offset: usize,
    hscroll_offset: usize,
    overview_state: WorkflowOverviewState,
) -> HorizontalLayout {
    let empty = horizontal_layout(area.width, 0, 0);
    // Less than one box row tall: no column can be drawn, so draw nothing
    // (not even edge markers).
    if area.width == 0 || area.height < STEP_BOX_HEIGHT || state.steps.is_empty() {
        return empty;
    }

    let columns = build_workflow_columns(state);
    let num_cols = columns.len();
    if num_cols == 0 {
        return empty;
    }

    let layout = horizontal_layout(area.width, num_cols, hscroll_offset);
    let gutter = overview_gutter(&layout, area.width);
    let base_col_w = layout.col_w;
    let first = layout.first;
    let end = first + layout.visible;
    // Right edge of the column area; the last visible column stretches to it.
    let area_right = area.x.saturating_add(area.width);
    let area_bottom = area.y.saturating_add(area.height);
    let cols_right = area_right.saturating_sub(gutter);

    // Edge markers for scrolled-out columns, on the arrows' row (the middle
    // row of the first box row), coloured by the worst news they hide.
    if gutter > 0 {
        let marker_y = area.y.saturating_add(1);
        if layout.hidden_left > 0 {
            let style = edge_marker_style(hidden_status_hint(&columns[..first]));
            frame.render_widget(
                Paragraph::new("\u{2039}").style(style),
                Rect::new(area.x, marker_y, 1, 1),
            );
        }
        if layout.hidden_right > 0 {
            let style = edge_marker_style(hidden_status_hint(&columns[end..]));
            frame.render_widget(
                Paragraph::new("\u{203a}").style(style),
                Rect::new(cols_right, marker_y, 1, 1),
            );
        }
    }

    // The number of vertical slots for parallel steps in this overview. The
    // minimized mode always draws exactly one row per stage.
    let visible_rows = if overview_state.is_maximized() {
        (area.height / STEP_BOX_HEIGHT).max(1) as usize
    } else {
        1
    };
    // Scrolling only means something when the stage does not fit; the
    // minimized overview is always exactly one row tall.
    let scroll_offset = if overview_state.is_maximized() {
        scroll_offset
    } else {
        0
    };

    let mut col_x = area.x.saturating_add(gutter);
    for (vis_idx, col_steps) in columns[first..end].iter().enumerate() {
        let is_last_visible = vis_idx + 1 == layout.visible;
        // Last visible column absorbs the remainder so the overview fills
        // the area (up to the right gutter).
        let this_col_w = if is_last_visible {
            cols_right.saturating_sub(col_x)
        } else {
            base_col_w
        };

        // Build the display rows for this stage. Maximized gives every step
        // its own row (steps beyond `max_concurrent` are marked queued);
        // minimized gives the stage a single row.
        let column_rows = if overview_state.is_maximized() {
            build_column_rows(col_steps, state.max_concurrent)
        } else {
            vec![build_minimized_row(col_steps)]
        };
        // A setup/teardown column is homogeneous by construction (see
        // `build_workflow_columns`), so its phase label applies to every
        // box in the column — the maximized-mode step box and the
        // minimized-mode `N steps…` summary alike — as a top-edge title
        // rather than text prepended to the step name.
        let column_title = match col_steps.first().map(|s| s.kind) {
            Some(WorkflowStepKind::Setup) => Some("[setup]".to_string()),
            Some(WorkflowStepKind::Teardown) => Some("[teardown]".to_string()),
            _ => None,
        };
        // When the stage does not fit, the last visible slot is spent on the
        // `+ N more…` marker rather than on a step box — so that slot's step
        // counts as hidden too.
        let remaining = column_rows.len().saturating_sub(scroll_offset);
        let shown = if remaining > visible_rows {
            visible_rows - 1
        } else {
            remaining
        };
        let hidden = remaining - shown;
        let rows_to_show: Vec<&ColumnRow> =
            column_rows.iter().skip(scroll_offset).take(shown).collect();

        for (row_idx, row) in rows_to_show.iter().enumerate() {
            // WI-0096 §11: truly-parallel siblings share the same box_x — no
            // per-row indent stagger (which used to imply sequential steps).
            let box_x = col_x;
            let box_w = this_col_w;
            let row_y = area
                .y
                .saturating_add((row_idx as u16).saturating_mul(STEP_BOX_HEIGHT));
            if row_y.saturating_add(STEP_BOX_HEIGHT) > area_bottom {
                break;
            }
            let box_area = Rect::new(box_x, row_y, box_w, STEP_BOX_HEIGHT);

            let (label, style, title) = match row {
                ColumnRow::Step { step, queued } => {
                    let is_current = state
                        .current_step
                        .as_ref()
                        .map(|c| c == &step.name)
                        .unwrap_or(false);
                    // Queued steps (waiting for a concurrency slot) get a `·`
                    // name prefix.
                    let name = if *queued {
                        format!("\u{00b7} {}", step.name)
                    } else {
                        step.name.clone()
                    };
                    let (label, style) =
                        step_box_label_and_style(&name, step.status, is_current, box_w);
                    let title = column_title.clone().or_else(|| {
                        step_agent_model_title(step.agent.as_deref(), step.model.as_deref(), box_w)
                    });
                    (label, style, title)
                }
                ColumnRow::Stage { count, status } => {
                    let name = format!("{count} steps\u{2026}");
                    // A stage summary stands for several steps that may run
                    // under different agents/models, so it carries no single
                    // agent/model label — press Ctrl-O to see them. A setup/
                    // teardown column keeps its phase label even collapsed.
                    let (label, style) = step_box_label_and_style(&name, *status, false, box_w);
                    (label, style, column_title.clone())
                }
            };

            let mut block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style);
            if let Some(title) = title {
                block = block.title(Span::styled(title, Style::default().fg(Color::DarkGray)));
            }
            let para = Paragraph::new(label).block(block).style(style);
            frame.render_widget(para, box_area);
        }

        // Arrow between this column and the next, on the middle row of the
        // FIRST row of boxes only (so it visually connects column headers
        // without overlapping parallel siblings). The first row always holds
        // a box: a step, or the `+ N more…` marker when no step fits. None
        // after the last visible column: either nothing follows it, or the
        // `›` edge marker takes the arrow's place.
        if !is_last_visible && (!rows_to_show.is_empty() || hidden > 0) {
            let arrow_x = col_x.saturating_add(this_col_w);
            if arrow_x < area_right {
                let arrow_area = Rect::new(arrow_x, area.y.saturating_add(1), 1, 1);
                frame.render_widget(
                    Paragraph::new("\u{2192}").style(Style::default().fg(Color::DarkGray)),
                    arrow_area,
                );
            }
        }

        // Overflow indicator below the last drawn box when there are hidden
        // steps under the fold. Scrolling the overview reveals them.
        if hidden > 0 {
            let row_y = area
                .y
                .saturating_add((rows_to_show.len() as u16).saturating_mul(STEP_BOX_HEIGHT));
            if row_y.saturating_add(STEP_BOX_HEIGHT) <= area_bottom {
                let box_w = this_col_w;
                let box_area = Rect::new(col_x, row_y, box_w, STEP_BOX_HEIGHT);
                let more_label = format!("+ {} more\u{2026}", hidden);
                let para = Paragraph::new(more_label)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::DarkGray)),
                    )
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(para, box_area);
            }
        }

        col_x = col_x.saturating_add(this_col_w).saturating_add(1);
    }

    layout
}

/// A single rendered row in a Workflow Overview column.
enum ColumnRow<'a> {
    /// One step's box. `queued` steps (beyond `max_concurrent`) get a `·`
    /// name prefix.
    Step {
        step: &'a WorkflowStepView,
        queued: bool,
    },
    /// The minimized-mode summary of a multi-step stage: `N steps…`, drawn in
    /// the stage's aggregate status colour.
    Stage {
        count: usize,
        status: StepViewStatus,
    },
}

/// Build the ordered display rows for one stage in **maximized** mode.
///
/// Every step keeps its own row, in workflow-definition order, for the whole
/// life of the run — completed siblings are never rolled up, and a step never
/// changes position as its neighbours finish.
///
/// Active steps beyond `max_concurrent` are marked `queued` (rendered with a
/// `·` prefix). `None` (unlimited) marks nothing.
fn build_column_rows<'a>(
    col: &[&'a WorkflowStepView],
    max_concurrent: Option<usize>,
) -> Vec<ColumnRow<'a>> {
    let mut active_idx = 0usize;
    col.iter()
        .map(|s| {
            let queued = if s.status.is_terminal() {
                false
            } else {
                let i = active_idx;
                active_idx += 1;
                matches!(max_concurrent, Some(mc) if i >= mc) && s.status == StepViewStatus::Pending
            };
            ColumnRow::Step { step: s, queued }
        })
        .collect()
}

/// Build the single **minimized**-mode row for one stage.
///
/// A one-step stage renders that step's normal box (name, agent/model title,
/// status colour). A parallel stage renders a `N steps…` summary instead.
fn build_minimized_row<'a>(col: &[&'a WorkflowStepView]) -> ColumnRow<'a> {
    if col.len() == 1 {
        return ColumnRow::Step {
            step: col[0],
            queued: false,
        };
    }
    ColumnRow::Stage {
        count: col.len(),
        status: stage_status(col),
    }
}

/// Aggregate a stage's steps into the one status its collapsed box shows.
///
/// Worst-news-first: a failure outranks a remediation, which outranks a
/// running step, which outranks anything still pending. Only once every step
/// is terminal does the stage read as finished — as `done` when they all
/// succeeded, otherwise as `cancelled` (the ⊘ glyph, since something was
/// cancelled or skipped).
fn stage_status(col: &[&WorkflowStepView]) -> StepViewStatus {
    let has = |s: StepViewStatus| col.iter().any(|step| step.status == s);
    if has(StepViewStatus::Error) {
        StepViewStatus::Error
    } else if has(StepViewStatus::Fixing) {
        StepViewStatus::Fixing
    } else if has(StepViewStatus::Running) {
        StepViewStatus::Running
    } else if col.iter().all(|s| s.status.is_terminal()) {
        if col.iter().all(|s| s.status == StepViewStatus::Done) {
            StepViewStatus::Done
        } else {
            StepViewStatus::Cancelled
        }
    } else {
        StepViewStatus::Pending
    }
}

/// Convert a `WorkflowState` (Layer 0 data) to a `WorkflowViewState` (TUI).
///
/// Prepends pseudo-steps from `setup_step_states`, maps main steps from
/// `steps` + `step_states`, and appends pseudo-steps from
/// `teardown_step_states`.
pub fn workflow_state_to_view_state(state: &WorkflowState) -> WorkflowViewState {
    let mut steps: Vec<WorkflowStepView> = Vec::new();

    for ps in &state.setup_step_states {
        steps.push(WorkflowStepView {
            name: ps.description.clone(),
            status: StepViewStatus::of_phase_step_status(&ps.status),
            agent: None,
            model: None,
            depends_on: Vec::new(),
            kind: WorkflowStepKind::Setup,
        });
    }

    for info in &state.steps {
        let status = state
            .step_states
            .get(&info.name)
            .map(StepViewStatus::of_step_state)
            .unwrap_or(StepViewStatus::Pending);
        steps.push(WorkflowStepView {
            name: info.name.clone(),
            status,
            agent: info.agent.clone(),
            model: info.model.clone(),
            depends_on: info.depends_on.clone(),
            kind: WorkflowStepKind::Agent,
        });
    }

    for ps in &state.teardown_step_states {
        steps.push(WorkflowStepView {
            name: ps.description.clone(),
            status: StepViewStatus::of_phase_step_status(&ps.status),
            agent: None,
            model: None,
            depends_on: Vec::new(),
            kind: WorkflowStepKind::Teardown,
        });
    }

    let current_step = state.current_step_index.and_then(|idx| {
        let setup_len = state.setup_step_states.len();
        state
            .steps
            .get(idx)
            .map(|s| s.name.clone())
            .or_else(|| steps.get(idx + setup_len).map(|s| s.name.clone()))
    });

    WorkflowViewState {
        steps,
        current_step,
        max_concurrent: None,
    }
}

/// Group steps into columns. Setup and teardown steps are never part of the
/// dependency DAG — they always run first and last — so they get their own
/// dedicated leading/trailing column rather than being grouped by topological
/// depth alongside the agent steps.
///
/// Agent steps are grouped by topological depth: steps at the same depth form
/// a parallel group (same column). Depth is the longest path from any root
/// (step with no dependencies) to this step. Steps that share the exact same
/// set of dependencies at the same depth are grouped together — steps that
/// depend on members of the previous parallel group all land in the next
/// column regardless of which specific member they depend on.
pub(crate) fn build_workflow_columns(state: &WorkflowViewState) -> Vec<Vec<&WorkflowStepView>> {
    use std::collections::HashMap;

    // Only agent steps participate in the `depends_on` DAG — a setup or
    // teardown pseudo-step's name is a free-text description, never a
    // dependency target.
    let step_names: HashMap<&str, usize> = state
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == WorkflowStepKind::Agent)
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();

    let mut depths: Vec<usize> = vec![0; state.steps.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (i, step) in state.steps.iter().enumerate() {
            if step.kind != WorkflowStepKind::Agent {
                continue;
            }
            for dep in &step.depends_on {
                if let Some(&dep_idx) = step_names.get(dep.as_str()) {
                    let new_depth = depths[dep_idx] + 1;
                    if new_depth > depths[i] {
                        depths[i] = new_depth;
                        changed = true;
                    }
                }
            }
        }
    }

    let agent_indices: Vec<usize> = state
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == WorkflowStepKind::Agent)
        .map(|(i, _)| i)
        .collect();
    let max_depth = agent_indices.iter().map(|&i| depths[i]).max().unwrap_or(0);

    let mut columns: Vec<Vec<&WorkflowStepView>> = Vec::with_capacity(max_depth + 3);

    let setup: Vec<&WorkflowStepView> = state
        .steps
        .iter()
        .filter(|s| s.kind == WorkflowStepKind::Setup)
        .collect();
    if !setup.is_empty() {
        columns.push(setup);
    }

    for d in 0..=max_depth {
        let col: Vec<&WorkflowStepView> = agent_indices
            .iter()
            .filter(|&&i| depths[i] == d)
            .map(|&i| &state.steps[i])
            .collect();
        if !col.is_empty() {
            columns.push(col);
        }
    }

    let teardown: Vec<&WorkflowStepView> = state
        .steps
        .iter()
        .filter(|s| s.kind == WorkflowStepKind::Teardown)
        .collect();
    if !teardown.is_empty() {
        columns.push(teardown);
    }

    columns
}

/// Build the top-border title for a step box — the `agent/model` the step will
/// run under (e.g. `claude/opus-4-8`).
///
/// Returns `None` when the step declares neither an agent nor a model: such a
/// step inherits the project-default agent AND model, so there is nothing that
/// distinguishes it and the box gets no title. When only one of the two is
/// known, just that part is shown. The result is truncated with an ellipsis to
/// fit `box_width`.
fn step_agent_model_title(
    agent: Option<&str>,
    model: Option<&str>,
    box_width: u16,
) -> Option<String> {
    let text = match (agent, model) {
        (None, None) => return None,
        (Some(a), Some(m)) => format!("{a}/{m}"),
        (Some(a), None) => a.to_string(),
        (None, Some(m)) => m.to_string(),
    };

    // Leave the two rounded corners of the top border untouched.
    let max_chars = (box_width as usize).saturating_sub(2).max(1);
    let title = if text.chars().count() > max_chars {
        let trunc: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{trunc}\u{2026}")
    } else {
        text
    };
    Some(title)
}

/// Compute the label text + style for a step box.
///
/// Status → glyph + color:
/// - Pending → `○` DarkGray
/// - Running → `●` Blue + Bold
/// - Done → `✓` Green
/// - Error → `✗` Red + Bold
/// - Fixing → `🔧` Magenta + Bold (on_failure remediation in progress)
/// - Cancelled / Skipped → `⊘` DarkGray
///
/// Current step is rendered with extra Bold on top of its status style.
/// Auto-advance-disabled steps get a small `🔒` prefix.
fn step_box_label_and_style(
    name: &str,
    status: StepViewStatus,
    is_current: bool,
    box_width: u16,
) -> (String, Style) {
    let max_name_chars = (box_width as usize).saturating_sub(6).max(1);
    let truncated_name = if name.chars().count() > max_name_chars {
        let trunc: String = name
            .chars()
            .take(max_name_chars.saturating_sub(1))
            .collect();
        format!("{trunc}\u{2026}")
    } else {
        name.to_string()
    };

    let (glyph, mut style) = match status {
        StepViewStatus::Pending => ("\u{25cb}", Style::default().fg(Color::DarkGray)),
        StepViewStatus::Running => (
            "\u{25cf}",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        StepViewStatus::Done => ("\u{2713}", Style::default().fg(Color::Green)),
        StepViewStatus::Error => (
            "\u{2717}",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        StepViewStatus::Fixing => (
            "\u{1f527}",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        StepViewStatus::Cancelled | StepViewStatus::Skipped => {
            ("\u{2298}", Style::default().fg(Color::DarkGray))
        }
    };
    if is_current {
        style = style.add_modifier(Modifier::BOLD);
    }
    let label = format!(" {glyph} {truncated_name} ");
    (label, style)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A status by the name the overview's own docs use. Unknown names panic
    /// rather than silently rendering as pending, which is what the `String`
    /// status did before WI 0114 F-22 typed it.
    fn st(name: &str) -> StepViewStatus {
        match name {
            "pending" => StepViewStatus::Pending,
            "running" => StepViewStatus::Running,
            "fixing" => StepViewStatus::Fixing,
            "done" => StepViewStatus::Done,
            "error" => StepViewStatus::Error,
            "cancelled" => StepViewStatus::Cancelled,
            "skipped" => StepViewStatus::Skipped,
            other => panic!("no such step status: {other}"),
        }
    }

    fn step(name: &str, status: &str, deps: Vec<&str>) -> WorkflowStepView {
        WorkflowStepView {
            name: name.into(),
            status: st(status),
            agent: None,
            model: None,
            depends_on: deps.into_iter().map(|s| s.into()).collect(),
            kind: WorkflowStepKind::Agent,
        }
    }

    fn phase_step(kind: WorkflowStepKind, name: &str, status: &str) -> WorkflowStepView {
        WorkflowStepView {
            name: name.into(),
            status: st(status),
            agent: None,
            model: None,
            depends_on: Vec::new(),
            kind,
        }
    }

    fn view(steps: Vec<WorkflowStepView>) -> WorkflowViewState {
        WorkflowViewState {
            steps,
            current_step: None,
            max_concurrent: None,
        }
    }

    #[test]
    fn build_workflow_columns_groups_by_topological_depth() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "running", vec!["a", "b"]),
        ]);
        let cols = build_workflow_columns(&v);
        // a + b at depth 0 → same column. c at depth 1 → next column.
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].len(), 2);
        assert_eq!(cols[1].len(), 1);
        assert_eq!(cols[1][0].name, "c");
    }

    #[test]
    fn build_workflow_columns_parallel_deps_land_same_column() {
        // D depends on B, E depends on C. Both B and C are at depth 1,
        // so D and E should both be at depth 2 (same column).
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec!["a"]),
            step("c", "done", vec!["a"]),
            step("d", "running", vec!["b"]),
            step("e", "running", vec!["c"]),
        ]);
        let cols = build_workflow_columns(&v);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].len(), 1); // a
        assert_eq!(cols[1].len(), 2); // b, c
        assert_eq!(cols[2].len(), 2); // d, e
    }

    #[test]
    fn build_workflow_columns_gives_setup_and_teardown_their_own_first_and_last_column() {
        let v = view(vec![
            phase_step(WorkflowStepKind::Setup, "clone repo", "done"),
            step("a", "done", vec![]),
            step("b", "running", vec!["a"]),
            phase_step(WorkflowStepKind::Teardown, "clean up", "pending"),
        ]);
        let cols = build_workflow_columns(&v);
        // setup | a | b | teardown — never merged with the first/last agent
        // column, regardless of the agent steps' own depth-0/depth-1 split.
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0].len(), 1);
        assert_eq!(cols[0][0].name, "clone repo");
        assert_eq!(cols[0][0].kind, WorkflowStepKind::Setup);
        assert_eq!(cols[1][0].name, "a");
        assert_eq!(cols[2][0].name, "b");
        assert_eq!(cols[3].len(), 1);
        assert_eq!(cols[3][0].name, "clean up");
        assert_eq!(cols[3][0].kind, WorkflowStepKind::Teardown);
    }

    #[test]
    fn build_workflow_columns_multiple_setup_steps_share_the_leading_column() {
        let v = view(vec![
            phase_step(WorkflowStepKind::Setup, "clone repo", "done"),
            phase_step(WorkflowStepKind::Setup, "install deps", "running"),
            step("a", "pending", vec![]),
        ]);
        let cols = build_workflow_columns(&v);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].len(), 2, "both setup steps share one column");
        assert_eq!(cols[1][0].name, "a");
    }

    const ROOMY: u16 = 200;

    fn maximized_height(v: &WorkflowViewState, max: u16) -> u16 {
        workflow_overview_height(v, WorkflowOverviewState::Maximized, max)
    }

    fn minimized_height(v: &WorkflowViewState, max: u16) -> u16 {
        workflow_overview_height(v, WorkflowOverviewState::Minimized, max)
    }

    #[test]
    fn workflow_overview_height_is_zero_when_no_steps() {
        let v = view(vec![]);
        assert_eq!(maximized_height(&v, ROOMY), 0);
        assert_eq!(minimized_height(&v, ROOMY), 0);
    }

    #[test]
    fn workflow_overview_height_3_when_sequential() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "running", vec!["a"]),
        ]);
        assert_eq!(maximized_height(&v, ROOMY), 3);
    }

    #[test]
    fn workflow_overview_height_grows_with_parallel_group() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "running", vec![]),
        ]);
        // 3 parallel steps → 3 * 3 = 9 rows.
        assert_eq!(maximized_height(&v, ROOMY), 9);
    }

    #[test]
    fn workflow_overview_height_maximized_has_no_cap_but_the_budget() {
        let steps: Vec<WorkflowStepView> = (0..40)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        // No concurrency cap and no legacy row cap: 40 parallel siblings each
        // get their own box when the frame is tall enough.
        assert_eq!(maximized_height(&v, ROOMY), 120);
    }

    #[test]
    fn workflow_overview_height_maximized_is_capped_to_available_space() {
        let steps: Vec<WorkflowStepView> = (0..40)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        // 20 rows of space → 6 whole boxes (18 rows); the overview never returns
        // a height that would clip a box mid-border.
        assert_eq!(maximized_height(&v, 20), 18);
    }

    #[test]
    fn workflow_overview_height_minimized_is_one_box_whatever_the_shape() {
        let steps: Vec<WorkflowStepView> = (0..40)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        assert_eq!(minimized_height(&v, ROOMY), 3);
    }

    #[test]
    fn workflow_overview_height_is_zero_when_not_even_one_box_fits() {
        let v = view(vec![step("a", "running", vec![])]);
        assert_eq!(minimized_height(&v, 2), 0);
        assert_eq!(maximized_height(&v, 2), 0);
    }

    // ── step_box_label_and_style ──────────────────────────────────────────────

    #[test]
    fn step_box_label_pending_uses_circle_glyph_and_dark_gray() {
        let (label, style) = step_box_label_and_style("foo", st("pending"), false, 20);
        assert!(label.contains('\u{25cb}'));
        assert!(label.contains("foo"));
        assert_eq!(style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn step_box_label_running_uses_filled_circle_blue_bold() {
        let (label, style) = step_box_label_and_style("foo", st("running"), false, 20);
        assert!(label.contains('\u{25cf}'));
        assert_eq!(style.fg, Some(Color::Blue));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_done_uses_check_glyph_green() {
        let (label, style) = step_box_label_and_style("foo", st("done"), false, 20);
        assert!(label.contains('\u{2713}'));
        assert_eq!(style.fg, Some(Color::Green));
    }

    #[test]
    fn step_box_label_error_uses_cross_glyph_red_bold() {
        let (label, style) = step_box_label_and_style("foo", st("error"), false, 20);
        assert!(label.contains('\u{2717}'));
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_current_step_adds_bold_on_top_of_status() {
        let (_, style) = step_box_label_and_style("foo", st("done"), true, 20);
        // Done is not bold by default, but is_current adds BOLD.
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    // ── step_agent_model_title ────────────────────────────────────────────────

    #[test]
    fn agent_model_title_none_when_neither_declared() {
        // No agent and no model → the step inherits the project defaults and
        // gets no title.
        assert_eq!(step_agent_model_title(None, None, 40), None);
    }

    #[test]
    fn agent_model_title_shows_agent_slash_model() {
        assert_eq!(
            step_agent_model_title(Some("claude"), Some("opus-4-8"), 40),
            Some("claude/opus-4-8".to_string())
        );
    }

    #[test]
    fn agent_model_title_agent_only() {
        assert_eq!(
            step_agent_model_title(Some("claude"), None, 40),
            Some("claude".to_string())
        );
    }

    #[test]
    fn agent_model_title_model_only() {
        assert_eq!(
            step_agent_model_title(None, Some("opus-4-8"), 40),
            Some("opus-4-8".to_string())
        );
    }

    #[test]
    fn agent_model_title_truncates_to_box_width() {
        let title = step_agent_model_title(Some("claude"), Some("opus-4-8"), 8).unwrap();
        assert!(title.chars().count() <= 6, "title should fit box_width - 2");
        assert!(title.contains('\u{2026}'));
    }

    #[test]
    fn step_box_label_truncates_long_name() {
        let (label, _) = step_box_label_and_style("very-long-step-name", st("pending"), false, 12);
        assert!(label.contains('\u{2026}'));
    }

    // ── WI-0096 §11 parallel overview rendering ────────────────────────────────

    #[test]
    fn parallel_siblings_share_one_column_no_row_indent() {
        // Three steps with the exact same (empty) dependency set are a parallel
        // group: they all land in the same column, which means the renderer
        // gives every one the same `box_x = col_x` — no per-row indent stagger.
        let v = view(vec![
            step("a", "running", vec![]),
            step("b", "running", vec![]),
            step("c", "running", vec![]),
        ]);
        let cols = build_workflow_columns(&v);
        assert_eq!(cols.len(), 1, "same dep-set siblings share a single column");
        assert_eq!(cols[0].len(), 3);
        let names: Vec<&str> = cols[0].iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn column_rows_never_roll_up_completed_parallel_siblings() {
        // A parallel group of 5 where 3 are completed keeps five rows: every
        // step holds its own box, in definition order, for the whole run.
        let steps = [
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "cancelled", vec![]),
            step("d", "running", vec![]),
            step("e", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        let rows = build_column_rows(&col, None);

        let names: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                ColumnRow::Step { step, .. } => step.name.as_str(),
                ColumnRow::Stage { .. } => panic!("expanded rows must never summarize a stage"),
            })
            .collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn minimized_row_for_single_step_stage_is_the_step_itself() {
        // A one-step stage is not a parallel group, so the minimized overview
        // draws its normal box — name, agent label, and all.
        let steps = [step("only", "running", vec![])];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        assert!(matches!(
            build_minimized_row(&col),
            ColumnRow::Step { step, queued: false } if step.name == "only"
        ));
    }

    #[test]
    fn minimized_row_for_parallel_stage_counts_the_steps() {
        let steps = [
            step("a", "done", vec![]),
            step("b", "running", vec![]),
            step("c", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        match build_minimized_row(&col) {
            ColumnRow::Stage { count, status } => {
                assert_eq!(count, 3);
                // One sibling is still running, so the stage reads as running.
                assert_eq!(status, st("running"));
            }
            _ => panic!("a parallel stage must collapse to a step-count summary"),
        }
    }

    #[test]
    fn stage_status_reports_worst_news_first() {
        let running = [step("a", "running", vec![]), step("b", "pending", vec![])];
        let failed = [step("a", "error", vec![]), step("b", "running", vec![])];
        let fixing = [step("a", "fixing", vec![]), step("b", "running", vec![])];
        let all_done = [step("a", "done", vec![]), step("b", "done", vec![])];
        let mixed_terminal = [step("a", "done", vec![]), step("b", "skipped", vec![])];
        let waiting = [step("a", "pending", vec![]), step("b", "pending", vec![])];

        let s = |steps: &[WorkflowStepView]| stage_status(&steps.iter().collect::<Vec<_>>());
        assert_eq!(s(&running), st("running"));
        assert_eq!(s(&failed), st("error"));
        assert_eq!(s(&fixing), st("fixing"));
        assert_eq!(s(&all_done), st("done"));
        assert_eq!(s(&mixed_terminal), st("cancelled"));
        assert_eq!(s(&waiting), st("pending"));
    }

    // ── overview renders agent/model title on the box border ───────────────────

    fn render_overview_text(v: &WorkflowViewState, width: u16, height: u16) -> String {
        render_overview_text_in(v, width, height, WorkflowOverviewState::Maximized)
    }

    fn render_overview_text_in(
        v: &WorkflowViewState,
        width: u16,
        height: u16,
        overview_state: WorkflowOverviewState,
    ) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workflow_overview(v, frame.area(), frame, 0, 0, overview_state);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn overview_shows_agent_model_title_for_overridden_step() {
        let mut s = step("build", "running", vec![]);
        s.agent = Some("claude".into());
        s.model = Some("opus-4-8".into());
        let v = view(vec![s]);
        let text = render_overview_text(&v, 40, 3);
        assert!(
            text.contains("claude/opus-4-8"),
            "expected agent/model title on the box border, got:\n{text}"
        );
    }

    #[test]
    fn overview_shows_setup_and_teardown_as_top_edge_titles_not_body_text() {
        let v = view(vec![
            phase_step(WorkflowStepKind::Setup, "clone repo", "done"),
            step("build", "running", vec![]),
            phase_step(WorkflowStepKind::Teardown, "clean up", "pending"),
        ]);
        let text = render_overview_text(&v, 80, 3);
        assert!(
            text.contains("[setup]"),
            "setup column must carry a [setup] title, got:\n{text}"
        );
        assert!(
            text.contains("[teardown]"),
            "teardown column must carry a [teardown] title, got:\n{text}"
        );
        assert!(
            !text.contains("[setup] clone repo") && !text.contains("[teardown] clean up"),
            "the phase label must not be prepended to the body text, got:\n{text}"
        );
        assert!(text.contains("clone repo"), "got:\n{text}");
        assert!(text.contains("clean up"), "got:\n{text}");
    }

    #[test]
    fn minimized_setup_column_keeps_its_title_when_collapsed_to_a_stage_summary() {
        let v = view(vec![
            phase_step(WorkflowStepKind::Setup, "clone repo", "done"),
            phase_step(WorkflowStepKind::Setup, "install deps", "running"),
            step("build", "pending", vec![]),
        ]);
        let text = render_overview_text_in(&v, 80, 3, WorkflowOverviewState::Minimized);
        assert!(
            text.contains("[setup]"),
            "collapsed setup stage must still carry its title, got:\n{text}"
        );
        assert!(text.contains("2 steps\u{2026}"), "got:\n{text}");
    }

    #[test]
    fn overview_omits_title_for_default_step() {
        // Neither agent nor model declared → box carries no agent/model title.
        let v = view(vec![step("build", "running", vec![])]);
        let text = render_overview_text(&v, 40, 3);
        assert!(
            !text.contains('/'),
            "default step should have no agent/model title, got:\n{text}"
        );
    }

    #[test]
    fn minimized_overview_shows_step_count_for_a_parallel_stage() {
        let v = view(vec![
            step("alpha", "running", vec![]),
            step("beta", "running", vec![]),
            step("gamma", "pending", vec![]),
        ]);
        let text = render_overview_text_in(&v, 40, 3, WorkflowOverviewState::Minimized);
        assert!(
            text.contains("3 steps\u{2026}"),
            "collapsed parallel stage must summarize as \"3 steps…\", got:\n{text}"
        );
        assert!(
            !text.contains("alpha"),
            "collapsed stage must not name individual steps, got:\n{text}"
        );
    }

    #[test]
    fn minimized_overview_shows_the_normal_box_for_a_single_step_stage() {
        let v = view(vec![
            step("build", "running", vec![]),
            step("test", "pending", vec!["build"]),
        ]);
        let text = render_overview_text_in(&v, 60, 3, WorkflowOverviewState::Minimized);
        assert!(text.contains("build"), "got:\n{text}");
        assert!(text.contains("test"), "got:\n{text}");
        assert!(!text.contains("steps\u{2026}"), "got:\n{text}");
    }

    #[test]
    fn maximized_overview_names_every_completed_parallel_sibling() {
        // The old overview rolled completed siblings into "(+N completed)". Now
        // each keeps its own named box.
        let v = view(vec![
            step("alpha", "done", vec![]),
            step("beta", "done", vec![]),
            step("gamma", "done", vec![]),
            step("delta", "running", vec![]),
        ]);
        let text = render_overview_text_in(&v, 40, 12, WorkflowOverviewState::Maximized);
        for name in ["alpha", "beta", "gamma", "delta"] {
            assert!(
                text.contains(name),
                "expected {name} in overview, got:\n{text}"
            );
        }
        assert!(!text.contains("completed)"), "got:\n{text}");
    }

    #[test]
    fn maximized_overview_keeps_the_agent_label_on_completed_siblings() {
        let mut a = step("alpha", "done", vec![]);
        a.agent = Some("claude".into());
        let mut b = step("beta", "done", vec![]);
        b.agent = Some("codex".into());
        let v = view(vec![a, b]);
        let text = render_overview_text_in(&v, 40, 6, WorkflowOverviewState::Maximized);
        assert!(text.contains("claude"), "got:\n{text}");
        assert!(text.contains("codex"), "got:\n{text}");
    }

    #[test]
    fn maximized_overview_overflows_into_a_more_box_when_the_frame_is_too_short() {
        let steps: Vec<WorkflowStepView> = (0..6)
            .map(|i| step(&format!("s{i}"), "running", vec![]))
            .collect();
        let v = view(steps);
        // 9 rows fit 3 boxes; the third becomes the overflow marker for the
        // 4 steps it hides.
        let text = render_overview_text_in(&v, 40, 9, WorkflowOverviewState::Maximized);
        assert!(
            text.contains("+ 4 more\u{2026}"),
            "expected an overflow box, got:\n{text}"
        );
    }

    #[test]
    fn column_rows_mark_steps_beyond_max_concurrent_as_queued() {
        // With max_concurrent = 2, pending siblings past the second are marked
        // queued (rendered with a `·` prefix).
        let steps = [
            step("a", "running", vec![]),
            step("b", "running", vec![]),
            step("c", "pending", vec![]),
            step("d", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        let rows = build_column_rows(&col, Some(2));
        let queued: Vec<bool> = rows
            .iter()
            .map(|r| matches!(r, ColumnRow::Step { queued, .. } if *queued))
            .collect();
        // a, b (running) not queued; c, d (pending, index >= 2) queued.
        assert_eq!(queued, vec![false, false, true, true]);
    }

    // ── WI 0118: horizontal scrolling ───────────────────────────────────

    fn layout(width: u16, n: usize, offset: usize) -> HorizontalLayout {
        horizontal_layout(width, n, offset)
    }

    /// Total cells the layout occupies: gutters, columns and arrow gaps.
    fn drawn_width(l: &HorizontalLayout, width: u16) -> usize {
        let gutter = overview_gutter(l, width) as usize;
        2 * gutter + l.visible * l.col_w as usize + l.visible.saturating_sub(1)
    }

    const EMPTY_LAYOUT: HorizontalLayout = HorizontalLayout {
        first: 0,
        visible: 0,
        col_w: 0,
        hidden_left: 0,
        hidden_right: 0,
    };

    #[test]
    fn horizontal_layout_fits_ignores_offset_and_shows_everything() {
        let l = layout(100, 3, 2);
        assert_eq!(l.first, 0);
        assert_eq!(l.visible, 3);
        assert_eq!(l.hidden_left, 0);
        assert_eq!(l.hidden_right, 0);
        assert!(!l.overflows());
        // Even split after the two arrow gaps.
        assert_eq!(l.col_w, (100 - 2) / 3);
    }

    #[test]
    fn horizontal_layout_fits_matches_the_old_even_split() {
        for n in 1usize..=6 {
            let min_fit = 19 * n as u16 - 1;
            for width in min_fit..=min_fit + 60 {
                let l = layout(width, n, 0);
                assert_eq!(l.visible, n, "width {width} n {n}");
                assert_eq!(
                    l.col_w,
                    width.saturating_sub(n as u16 - 1) / n as u16,
                    "width {width} n {n}"
                );
                assert!(l.col_w >= MIN_COLUMN_WIDTH);
                assert!(!l.overflows());
            }
        }
    }

    #[test]
    fn horizontal_layout_overflow_at_common_widths_with_14_columns() {
        // (width, expected visible)
        for (width, expected_visible) in [(80u16, 4usize), (100, 5), (120, 6), (200, 10)] {
            let l = layout(width, 14, 0);
            assert_eq!(l.visible, expected_visible, "width {width}");
            assert!(l.col_w >= MIN_COLUMN_WIDTH, "width {width}: {l:?}");
            assert!(drawn_width(&l, width) <= width as usize, "width {width}");
            assert!(l.overflows());
            assert_eq!(l.first, 0);
            assert_eq!(l.hidden_left, 0);
            assert_eq!(l.hidden_right, 14 - expected_visible);
        }
    }

    #[test]
    fn horizontal_layout_100_wide_14_columns_is_five_visible_at_minimum_width() {
        let l = layout(100, 14, 0);
        assert_eq!(l.visible, 5);
        assert_eq!(l.col_w, MIN_COLUMN_WIDTH);
    }

    #[test]
    fn horizontal_layout_offset_past_the_end_clamps_to_last_full_page() {
        let l = layout(100, 14, 999);
        assert_eq!(l.visible, 5);
        assert_eq!(l.first, 14 - 5);
        assert_eq!(l.hidden_left, 9);
        assert_eq!(l.hidden_right, 0);
        assert!(l.overflows());
    }

    #[test]
    fn horizontal_layout_offset_exactly_at_the_last_page_is_kept() {
        let l = layout(100, 14, 9);
        assert_eq!(l.first, 9);
        assert_eq!(l.hidden_right, 0);
        let l = layout(100, 14, 10);
        assert_eq!(l.first, 9);
    }

    #[test]
    fn horizontal_layout_offset_zero_has_no_hidden_left() {
        let l = layout(100, 14, 0);
        assert_eq!(l.hidden_left, 0);
        assert_eq!(l.hidden_right, 9);
    }

    #[test]
    fn horizontal_layout_mid_offset_hides_both_sides() {
        let l = layout(100, 14, 3);
        assert_eq!(l.first, 3);
        assert_eq!(l.hidden_left, 3);
        assert_eq!(l.hidden_right, 14 - 5 - 3);
        assert_eq!(l.first + l.visible + l.hidden_right, 14);
    }

    #[test]
    fn horizontal_layout_fits_at_exactly_n_times_19_minus_1() {
        for n in 2usize..=20 {
            let fit_at = 19 * n as u16 - 1;
            let fits = layout(fit_at, n, 0);
            assert!(!fits.overflows(), "n {n} width {fit_at}");
            assert_eq!(fits.visible, n);
            assert_eq!(fits.col_w, MIN_COLUMN_WIDTH, "every column exactly minimum");

            let over = layout(fit_at - 1, n, 0);
            assert!(over.overflows(), "n {n} width {}", fit_at - 1);
            assert!(over.visible < n);
            assert!(over.col_w >= MIN_COLUMN_WIDTH);
        }
    }

    #[test]
    fn horizontal_layout_two_columns_threshold_is_37() {
        assert!(!layout(37, 2, 0).overflows());
        assert!(layout(36, 2, 0).overflows());
    }

    #[test]
    fn horizontal_layout_tiny_widths_show_one_column_without_underflow() {
        for width in [1u16, 2, 10, 19] {
            let l = layout(width, 5, 0);
            assert_eq!(l.visible, 1, "width {width}");
            assert_eq!(l.col_w, width, "width {width}");
            assert_eq!(l.first, 0);
            assert_eq!(l.hidden_left, 0);
            assert_eq!(l.hidden_right, 4);
            assert_eq!(overview_gutter(&l, width), 0, "no gutters when tiny");
            assert!(drawn_width(&l, width) <= width as usize);
        }
    }

    #[test]
    fn horizontal_layout_tiny_width_can_still_scroll_one_column_at_a_time() {
        let l = layout(10, 5, 3);
        assert_eq!(l.first, 3);
        assert_eq!(l.visible, 1);
        assert_eq!(l.hidden_left, 3);
        assert_eq!(l.hidden_right, 1);
        let l = layout(10, 5, 99);
        assert_eq!(l.first, 4);
        assert_eq!(l.hidden_right, 0);
    }

    #[test]
    fn horizontal_layout_width_zero_is_empty() {
        assert_eq!(layout(0, 5, 0), EMPTY_LAYOUT);
        assert_eq!(layout(0, 5, 3), EMPTY_LAYOUT);
        assert_eq!(layout(0, 1, 0), EMPTY_LAYOUT);
    }

    #[test]
    fn horizontal_layout_zero_columns_is_empty() {
        for width in [0u16, 1, 20, 100, u16::MAX] {
            assert_eq!(layout(width, 0, 0), EMPTY_LAYOUT, "width {width}");
            assert_eq!(layout(width, 0, 7), EMPTY_LAYOUT, "width {width}");
        }
    }

    #[test]
    fn horizontal_layout_single_column_never_overflows() {
        for width in [1u16, 2, 10, 17, 18, 19, 20, 100, 200] {
            let l = layout(width, 1, 5);
            assert_eq!(l.visible, 1, "width {width}");
            assert_eq!(l.first, 0);
            assert_eq!(l.col_w, width);
            assert!(!l.overflows(), "width {width}");
        }
    }

    #[test]
    fn horizontal_layout_minimum_overflow_width_shows_one_column_with_gutters() {
        // 20 = one 18-wide column + two gutters.
        let l = layout(20, 2, 0);
        assert_eq!(l.visible, 1);
        assert_eq!(l.col_w, MIN_COLUMN_WIDTH);
        assert_eq!(l.hidden_right, 1);
        assert_eq!(overview_gutter(&l, 20), 1);
        // One narrower and the gutters are dropped.
        let l = layout(19, 2, 0);
        assert_eq!(l.col_w, 19);
        assert_eq!(overview_gutter(&l, 19), 0);
    }

    #[test]
    fn horizontal_layout_more_columns_than_u16_always_overflows() {
        let l = layout(100, 70_000, 0);
        assert!(l.overflows());
        assert_eq!(l.visible, 5);
        assert_eq!(l.hidden_right, 70_000 - 5);
    }

    #[test]
    fn horizontal_layout_maximum_width_does_not_overflow_arithmetic() {
        let l = layout(u16::MAX, 5, 0);
        assert_eq!(l.visible, 5);
        assert!(!l.overflows());
        let l = layout(u16::MAX, 5000, 0);
        assert!(l.overflows());
        assert!(drawn_width(&l, u16::MAX) <= u16::MAX as usize);
    }

    #[test]
    fn horizontal_layout_invariants_hold_across_a_sweep() {
        for width in 0u16..=300 {
            for n in 0usize..=20 {
                for offset in [0usize, 1, 3, 7, 100] {
                    let l = layout(width, n, offset);
                    let ctx = format!("width {width} n {n} offset {offset}: {l:?}");
                    if n == 0 || width == 0 {
                        assert_eq!(l, EMPTY_LAYOUT, "{ctx}");
                        continue;
                    }
                    assert!(l.visible >= 1 && l.visible <= n, "{ctx}");
                    assert_eq!(l.hidden_left, l.first, "{ctx}");
                    assert_eq!(l.first + l.visible + l.hidden_right, n, "{ctx}");
                    assert!(drawn_width(&l, width) <= width as usize, "{ctx}");
                    // Only a lone column squeezed into a tiny width may be
                    // narrower than the minimum.
                    if l.visible > 1 || width >= MIN_COLUMN_WIDTH {
                        assert!(l.col_w >= MIN_COLUMN_WIDTH, "{ctx}");
                    }
                    if !l.overflows() {
                        assert_eq!(l.first, 0, "{ctx}");
                        assert_eq!(l.visible, n, "{ctx}");
                    }
                }
            }
        }
    }

    // ── hidden_status_hint ──────────────────────────────────────────────

    fn hint_of(statuses: &[&[&str]]) -> &'static str {
        let owned: Vec<Vec<WorkflowStepView>> = statuses
            .iter()
            .enumerate()
            .map(|(c, col)| {
                col.iter()
                    .enumerate()
                    .map(|(r, s)| step(&format!("s{c}-{r}"), s, vec![]))
                    .collect()
            })
            .collect();
        let cols: Vec<Vec<&WorkflowStepView>> = owned.iter().map(|c| c.iter().collect()).collect();
        hidden_status_hint(&cols)
    }

    #[test]
    fn hidden_status_hint_empty_input_is_plain() {
        assert_eq!(hidden_status_hint(&[]), "plain");
        assert_eq!(hidden_status_hint(&[Vec::new()]), "plain");
    }

    #[test]
    fn hidden_status_hint_plain_when_nothing_notable() {
        assert_eq!(hint_of(&[&["done"], &["pending", "skipped"]]), "plain");
        assert_eq!(hint_of(&[&["cancelled"]]), "plain");
    }

    #[test]
    fn hidden_status_hint_running_beats_plain() {
        assert_eq!(hint_of(&[&["done"], &["pending", "running"]]), "running");
    }

    #[test]
    fn hidden_status_hint_error_beats_running_and_plain() {
        assert_eq!(hint_of(&[&["running"], &["error"]]), "error");
        assert_eq!(hint_of(&[&["error", "running", "done"]]), "error");
        assert_eq!(hint_of(&[&["done"], &["error"], &["pending"]]), "error");
    }

    #[test]
    fn hidden_status_hint_fixing_alone_is_plain_but_fixing_with_running_is_running() {
        assert_eq!(hint_of(&[&["fixing"]]), "plain");
        assert_eq!(hint_of(&[&["fixing"], &["running"]]), "running");
    }

    #[test]
    fn edge_marker_style_matches_the_hint() {
        assert_eq!(
            edge_marker_style("error"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            edge_marker_style("running"),
            Style::default().fg(Color::Blue)
        );
        assert_eq!(
            edge_marker_style("plain"),
            Style::default().fg(Color::DarkGray)
        );
    }

    // ── follow helpers ──────────────────────────────────────────────────

    /// A chain of `n` sequential agent steps `s0 → s1 → …`, one per column.
    fn chain(n: usize, statuses: &[(usize, &str)]) -> WorkflowViewState {
        let steps = (0..n)
            .map(|i| {
                let status = statuses
                    .iter()
                    .find(|(idx, _)| *idx == i)
                    .map_or("pending", |(_, s)| s);
                let deps = if i == 0 {
                    vec![]
                } else {
                    vec![format!("s{}", i - 1)]
                };
                let mut s = step(&format!("s{i}"), status, vec![]);
                s.depends_on = deps;
                s
            })
            .collect();
        view(steps)
    }

    #[test]
    fn follow_column_uses_current_step_column() {
        let mut v = chain(14, &[(2, "running")]);
        v.current_step = Some("s9".into());
        let cols = build_workflow_columns(&v);
        assert_eq!(follow_column(&v, &cols), Some(9));
    }

    #[test]
    fn follow_column_falls_back_to_first_running_step() {
        let v = chain(14, &[(6, "running"), (8, "running")]);
        let cols = build_workflow_columns(&v);
        assert_eq!(follow_column(&v, &cols), Some(6));
    }

    #[test]
    fn follow_column_is_none_with_nothing_current_or_running() {
        let v = chain(5, &[(0, "done")]);
        let cols = build_workflow_columns(&v);
        assert_eq!(follow_column(&v, &cols), None);
    }

    #[test]
    fn follow_column_falls_back_for_a_running_setup_phase() {
        let v = view(vec![
            phase_step(WorkflowStepKind::Setup, "clone repo", "running"),
            step("a", "pending", vec![]),
        ]);
        let cols = build_workflow_columns(&v);
        assert_eq!(follow_column(&v, &cols), Some(0));
    }

    #[test]
    fn follow_column_falls_back_for_a_running_teardown_phase() {
        let mut v = view(vec![
            step("build", "done", vec![]),
            phase_step(WorkflowStepKind::Teardown, "clean up", "running"),
        ]);
        // A resumed snapshot can still name the completed agent.
        v.current_step = Some("build".into());
        let cols = build_workflow_columns(&v);
        assert_eq!(follow_column(&v, &cols), Some(1));
        assert_eq!(follow_hscroll_offset(20, cols.len(), 0, Some(1)), 1);
    }

    #[test]
    fn follow_column_current_step_matches_agent_steps_only() {
        // A teardown step named like the current agent step must not win.
        let mut v = view(vec![
            step("deploy", "running", vec![]),
            phase_step(WorkflowStepKind::Teardown, "deploy", "pending"),
        ]);
        v.current_step = Some("deploy".into());
        let cols = build_workflow_columns(&v);
        assert_eq!(follow_column(&v, &cols), Some(0));
    }

    #[test]
    fn follow_hscroll_offset_keeps_offset_when_column_visible() {
        // width 100 / 14 columns: 5 visible.
        assert_eq!(follow_hscroll_offset(100, 14, 3, Some(3)), 3);
        assert_eq!(follow_hscroll_offset(100, 14, 3, Some(7)), 3);
    }

    #[test]
    fn follow_hscroll_offset_moves_left_to_the_column() {
        assert_eq!(follow_hscroll_offset(100, 14, 6, Some(2)), 2);
    }

    #[test]
    fn follow_hscroll_offset_moves_right_so_the_column_is_last_in_view() {
        assert_eq!(follow_hscroll_offset(100, 14, 0, Some(8)), 8 + 1 - 5);
        assert_eq!(follow_hscroll_offset(100, 14, 0, Some(13)), 9);
    }

    #[test]
    fn follow_hscroll_offset_without_a_column_keeps_the_offset() {
        assert_eq!(follow_hscroll_offset(100, 14, 4, None), 4);
    }

    #[test]
    fn follow_hscroll_offset_on_an_empty_layout_keeps_the_offset() {
        assert_eq!(follow_hscroll_offset(0, 14, 4, Some(9)), 4);
        assert_eq!(follow_hscroll_offset(100, 0, 4, Some(9)), 4);
    }

    #[test]
    fn hscroll_follow_reattaches_when_everything_fits() {
        assert!(hscroll_follow_reattaches(&layout(200, 3, 0), None));
        assert!(hscroll_follow_reattaches(&layout(200, 3, 0), Some(1)));
    }

    #[test]
    fn hscroll_follow_reattaches_at_the_right_end_only_with_the_column_in_view() {
        let at_end = layout(100, 14, 99);
        assert_eq!(at_end.hidden_right, 0);
        assert!(hscroll_follow_reattaches(&at_end, Some(12)));
        assert!(!hscroll_follow_reattaches(&at_end, Some(2)));
        assert!(!hscroll_follow_reattaches(&at_end, None));
    }

    #[test]
    fn hscroll_follow_does_not_reattach_while_columns_are_hidden_on_the_right() {
        let mid = layout(100, 14, 2);
        assert!(!hscroll_follow_reattaches(&mid, Some(3)));
    }

    #[test]
    fn hscroll_follow_never_reattaches_on_a_degenerate_layout() {
        assert!(!hscroll_follow_reattaches(&EMPTY_LAYOUT, Some(0)));
        assert!(!hscroll_follow_reattaches(&layout(0, 5, 0), None));
    }

    // ── rendering with horizontal scroll ────────────────────────────────

    /// Render into a `width`×`height` terminal, returning the text, the
    /// buffer and the resolved layout.
    fn render_hscroll(
        v: &WorkflowViewState,
        width: u16,
        height: u16,
        hscroll: usize,
    ) -> (String, ratatui::buffer::Buffer, HorizontalLayout) {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut resolved = EMPTY_LAYOUT;
        terminal
            .draw(|frame| {
                resolved = render_workflow_overview(
                    v,
                    frame.area(),
                    frame,
                    0,
                    hscroll,
                    WorkflowOverviewState::Minimized,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text = (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buf, resolved)
    }

    #[test]
    fn render_returns_the_resolved_layout() {
        let v = chain(14, &[]);
        let (_, _, l) = render_hscroll(&v, 100, 3, 99);
        assert_eq!(l, layout(100, 14, 99));
    }

    #[test]
    fn render_overflow_shows_right_marker_only_at_offset_zero() {
        let v = chain(14, &[]);
        let (text, _, l) = render_hscroll(&v, 100, 3, 0);
        assert_eq!(l.visible, 5);
        assert!(text.contains('\u{203a}'), "missing ›:\n{text}");
        assert!(!text.contains('\u{2039}'), "unexpected ‹:\n{text}");
        for name in ["s0", "s1", "s2", "s3", "s4"] {
            assert!(text.contains(name), "missing {name}:\n{text}");
        }
        assert!(!text.contains("s5"), "s5 should be hidden:\n{text}");
    }

    #[test]
    fn render_mid_scroll_shows_both_markers() {
        let v = chain(14, &[]);
        let (text, _, l) = render_hscroll(&v, 100, 3, 4);
        assert_eq!(l.first, 4);
        assert!(text.contains('\u{2039}'), "missing ‹:\n{text}");
        assert!(text.contains('\u{203a}'), "missing ›:\n{text}");
        assert!(text.contains("s4") && text.contains("s8"), "{text}");
        assert!(!text.contains("s3") && !text.contains("s9"), "{text}");
    }

    #[test]
    fn render_scrolled_to_the_end_shows_only_the_left_marker() {
        let v = chain(14, &[]);
        let (text, _, l) = render_hscroll(&v, 100, 3, 999);
        assert_eq!(l.hidden_right, 0);
        assert!(text.contains('\u{2039}'), "{text}");
        assert!(!text.contains('\u{203a}'), "{text}");
        assert!(text.contains("s13"), "{text}");
    }

    #[test]
    fn render_scrolled_phase_columns_keep_their_titles() {
        let v = view(vec![
            phase_step(WorkflowStepKind::Setup, "prepare", "done"),
            step("build", "done", vec![]),
            phase_step(WorkflowStepKind::Teardown, "clean up", "running"),
        ]);
        let (first, _, _) = render_hscroll(&v, 20, 3, 0);
        let (last, _, layout) = render_hscroll(&v, 20, 3, 2);
        assert!(first.contains("[setup]"), "{first}");
        assert!(last.contains("[teardown]"), "{last}");
        assert_eq!(layout.first, 2);
    }

    #[test]
    fn horizontal_scroll_preserves_parallel_queue_and_vertical_more_box() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut v = view(vec![
            step("root", "done", vec![]),
            step("a", "running", vec!["root"]),
            step("b", "running", vec!["root"]),
            step("c", "pending", vec!["root"]),
            step("d", "pending", vec!["root"]),
            step("tail", "pending", vec!["a"]),
        ]);
        v.max_concurrent = Some(2);
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        let mut draw = |vertical_offset| {
            terminal
                .draw(|frame| {
                    render_workflow_overview(
                        &v,
                        frame.area(),
                        frame,
                        vertical_offset,
                        1,
                        WorkflowOverviewState::Maximized,
                    );
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            (0..buf.area().height)
                .flat_map(|y| {
                    (0..buf.area().width)
                        .map(move |x| buf.cell((x, y)).unwrap().symbol().to_string())
                })
                .collect::<String>()
        };
        let first = draw(0);
        assert!(first.contains("+ 3 more…"), "{first}");
        let scrolled = draw(2);
        assert!(
            scrolled.contains("· c") && scrolled.contains("· d"),
            "{scrolled}"
        );
    }

    #[test]
    fn render_markers_sit_on_the_middle_row_at_the_area_edges() {
        let v = chain(14, &[]);
        let (_, buf, _) = render_hscroll(&v, 100, 3, 4);
        assert_eq!(buf.cell((0, 1)).unwrap().symbol(), "\u{2039}");
        assert_eq!(buf.cell((99, 1)).unwrap().symbol(), "\u{203a}");
    }

    #[test]
    fn render_marker_is_red_when_a_hidden_column_failed() {
        let v = chain(14, &[(10, "error")]);
        let (_, buf, _) = render_hscroll(&v, 100, 3, 0);
        let marker = buf.cell((99, 1)).unwrap();
        assert_eq!(marker.symbol(), "\u{203a}");
        assert_eq!(marker.fg, Color::Red);
        assert!(marker.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn render_marker_is_blue_for_a_hidden_running_step_and_gray_otherwise() {
        let running = chain(14, &[(10, "running")]);
        let (_, buf, _) = render_hscroll(&running, 100, 3, 0);
        assert_eq!(buf.cell((99, 1)).unwrap().fg, Color::Blue);

        let quiet = chain(14, &[]);
        let (_, buf, _) = render_hscroll(&quiet, 100, 3, 0);
        assert_eq!(buf.cell((99, 1)).unwrap().fg, Color::DarkGray);
    }

    #[test]
    fn render_left_marker_reflects_only_left_hidden_columns() {
        let v = chain(14, &[(1, "error")]);
        let (_, buf, _) = render_hscroll(&v, 100, 3, 4);
        assert_eq!(buf.cell((0, 1)).unwrap().fg, Color::Red);
        assert_ne!(buf.cell((99, 1)).unwrap().fg, Color::Red);
    }

    #[test]
    fn render_workflow_that_fits_has_no_markers_and_ignores_offset() {
        let v = chain(3, &[]);
        let (a, _, la) = render_hscroll(&v, 100, 3, 0);
        let (b, _, lb) = render_hscroll(&v, 100, 3, 7);
        assert_eq!(a, b);
        assert_eq!(la, lb);
        assert!(!a.contains('\u{2039}') && !a.contains('\u{203a}'), "{a}");
        assert!(a.contains("s0") && a.contains("s1") && a.contains("s2"));
    }

    #[test]
    fn render_arrow_is_drawn_between_visible_columns_but_not_after_the_last() {
        let v = chain(14, &[]);
        let (text, _, l) = render_hscroll(&v, 100, 3, 0);
        let middle = text.lines().nth(1).unwrap();
        let arrows = middle.matches('\u{2192}').count();
        assert_eq!(arrows, l.visible - 1, "{middle}");
    }

    #[test]
    fn render_tiny_widths_do_not_panic_or_draw_markers() {
        let v = chain(14, &[(3, "error")]);
        for width in [0u16, 1, 2, 5, 10, 19] {
            let (text, _, l) = render_hscroll(&v, width, 3, 5);
            assert!(
                !text.contains('\u{2039}') && !text.contains('\u{203a}'),
                "{width}"
            );
            if width == 0 {
                assert_eq!(l, EMPTY_LAYOUT);
            } else {
                assert_eq!(l.visible, 1, "width {width}");
            }
        }
    }

    #[test]
    fn render_too_short_area_draws_nothing_and_returns_an_empty_layout() {
        let v = chain(14, &[]);
        for height in [1u16, 2] {
            let (text, _, l) = render_hscroll(&v, 100, height, 4);
            assert_eq!(l, EMPTY_LAYOUT, "height {height}");
            assert!(text.trim().is_empty(), "height {height}:\n{text}");
        }
    }

    #[test]
    fn render_empty_workflow_returns_an_empty_layout() {
        let v = view(vec![]);
        let (_, _, l) = render_hscroll(&v, 100, 3, 0);
        assert_eq!(l, EMPTY_LAYOUT);
    }

    #[test]
    fn render_draws_nothing_outside_the_given_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let v = chain(14, &[(10, "error")]);
        let area = Rect::new(5, 2, 60, 3);
        for hscroll in [0usize, 4, 99] {
            let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
            terminal
                .draw(|frame| {
                    render_workflow_overview(
                        &v,
                        area,
                        frame,
                        0,
                        hscroll,
                        WorkflowOverviewState::Minimized,
                    );
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            for y in 0..8u16 {
                for x in 0..80u16 {
                    let inside = x >= area.x
                        && x < area.x + area.width
                        && y >= area.y
                        && y < area.y + area.height;
                    if !inside {
                        assert_eq!(
                            buf.cell((x, y)).unwrap().symbol(),
                            " ",
                            "cell ({x},{y}) drawn outside area at hscroll {hscroll}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn render_shows_full_step_names_at_the_minimum_column_width() {
        let steps = (0..14)
            .map(|i| {
                let mut s = step(&format!("implement-{i}"), "pending", vec![]);
                if i > 0 {
                    s.depends_on = vec![format!("implement-{}", i - 1)];
                }
                s
            })
            .collect();
        let v = view(steps);
        let (text, _, l) = render_hscroll(&v, 100, 3, 0);
        assert_eq!(l.visible, 5);
        for i in 0..5 {
            assert!(text.contains(&format!("implement-{i}")), "{text}");
        }
    }

    #[test]
    fn render_fourteen_step_chain_never_panics_at_any_width_or_offset() {
        let v = chain(14, &[(0, "done"), (5, "running"), (9, "error")]);
        for width in (0u16..=120).step_by(7).chain([200, 300]) {
            for offset in [0usize, 1, 6, 13, 99] {
                for height in [0u16, 1, 3, 6] {
                    let (_, _, l) = render_hscroll(&v, width, height, offset);
                    assert!(l.first + l.visible + l.hidden_right <= 14);
                }
            }
        }
    }
}
