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

use crate::data::workflow_state::group_stages;
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

            let mut reported: Option<String> = None;
            let (label, mut style, title) = match row {
                ColumnRow::Step { step, queued } => {
                    reported = state.reported.get(&step.name).cloned();
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
                    let status = if col_steps
                        .iter()
                        .any(|step| state.reported.get(&step.name).is_some_and(|s| s == "error"))
                    {
                        StepViewStatus::Error
                    } else {
                        *status
                    };
                    // A stage summary stands for several steps that may run
                    // under different agents/models, so it carries no single
                    // agent/model label — press Ctrl-O to see them. A setup/
                    // teardown column keeps its phase label even collapsed.
                    let (label, style) = step_box_label_and_style(&name, status, false, box_w);
                    (label, style, column_title.clone())
                }
            };

            if reported.as_deref() == Some("error") {
                style = style.fg(Color::Red);
            }

            let mut block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style);
            if let Some(title) = title {
                block = block.title(Span::styled(title, Style::default().fg(Color::DarkGray)));
            }
            // The agent's reported problem (`blocked: permission`, `error`) sits on
            // the bottom edge of the step box.
            if let Some(text) = reported {
                let color = if text == "error" {
                    Color::Red
                } else {
                    Color::Yellow
                };
                block = block.title_bottom(Span::styled(
                    format!(" {text} "),
                    Style::default().fg(color),
                ));
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

/// Pure data projection, now Layer 2 (`command::instance::feeds`) so the
/// command thread can publish it; re-exported for the renderer and its tests.
pub use crate::command::instance::feeds::workflow_state_to_view_state;

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
    // Only agent steps participate in the `depends_on` DAG — a setup or
    // teardown pseudo-step's name is a free-text description, never a
    // dependency target.
    let agent_indices: Vec<usize> = state
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == WorkflowStepKind::Agent)
        .map(|(i, _)| i)
        .collect();
    let deps: Vec<(&str, &[String])> = agent_indices
        .iter()
        .map(|&i| {
            (
                state.steps[i].name.as_str(),
                state.steps[i].depends_on.as_slice(),
            )
        })
        .collect();

    let mut columns: Vec<Vec<&WorkflowStepView>> = Vec::new();

    let setup: Vec<&WorkflowStepView> = state
        .steps
        .iter()
        .filter(|s| s.kind == WorkflowStepKind::Setup)
        .collect();
    if !setup.is_empty() {
        columns.push(setup);
    }

    for stage in group_stages(&deps) {
        columns.push(
            stage
                .into_iter()
                .map(|k| &state.steps[agent_indices[k]])
                .collect(),
        );
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
