//! Workflow status strip — horizontal display of workflow step progression.
//!
//! Layout matches old amux:
//! - Steps are grouped into **topological columns** by sorted `depends_on`
//!   signature (steps that share the same dependencies sit in the same
//!   column).
//! - Each step renders as a **3-row rounded box** with a status glyph and
//!   the step name.
//! - Parallel siblings (multiple steps in the same column) **stack
//!   vertically with a 1-cell indent per row** to imply they will run
//!   sequentially.
//! - **Inter-column `→` arrows** sit on the middle row of the first row of
//!   boxes, joining adjacent columns.
//! - When more parallel steps exist than rows fit, the last visible row
//!   becomes a `+ N more…` overflow box.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::data::workflow_state::{PhaseStepStatus, StepState, WorkflowState};
use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};

/// Compute the rows needed for the workflow strip given a view state.
///
/// Each box is 3 rows tall. The number of visible parallel rows is capped at
/// the effective `max_concurrent` (WI-0096 §11) — `None` (unlimited) keeps the
/// legacy cap of 3, so single-container / sequential workflows are unchanged.
/// Returns 0 when `state` is empty / has no steps.
pub fn workflow_strip_height(state: &WorkflowViewState) -> u16 {
    if state.steps.is_empty() {
        return 0;
    }
    let columns = build_workflow_columns(state);
    let max_parallel = columns.iter().map(|c| c.len()).max().unwrap_or(1);
    let cap = state.max_concurrent.unwrap_or(max_parallel);
    let rows = max_parallel.min(cap).max(1) as u16;
    rows * 3
}

/// Render the workflow status strip into the given area.
pub fn render_workflow_strip(
    state: &WorkflowViewState,
    area: Rect,
    frame: &mut Frame,
    scroll_offset: usize,
) {
    if area.width == 0 || area.height == 0 || state.steps.is_empty() {
        return;
    }

    let columns = build_workflow_columns(state);
    let num_cols = columns.len();
    if num_cols == 0 {
        return;
    }

    // Subtract one cell per inter-column arrow gap.
    let arrow_chars = num_cols.saturating_sub(1) as u16;
    let box_space = area.width.saturating_sub(arrow_chars);
    let base_col_w = (box_space / num_cols as u16).max(4);

    // The number of vertical slots for parallel steps in this strip.
    let visible_rows = (area.height / 3).max(1) as usize;

    let mut col_x = area.x;
    for (col_idx, col_steps) in columns.iter().enumerate() {
        // Last column absorbs the remainder so the strip fills the area.
        let this_col_w = if col_idx + 1 == num_cols {
            area.x + area.width - col_x
        } else {
            base_col_w
        };

        // WI-0096 §11: build the display rows for this column — completed
        // siblings in a parallel group collapse into a single "(+N completed)"
        // summary row, and steps beyond `max_concurrent` are marked queued.
        let column_rows = build_column_rows(col_steps, state.max_concurrent);
        let rows_to_show: Vec<&ColumnRow> = column_rows
            .iter()
            .skip(scroll_offset)
            .take(visible_rows)
            .collect();
        let hidden = column_rows
            .len()
            .saturating_sub(scroll_offset + visible_rows);

        for (row_idx, row) in rows_to_show.iter().enumerate() {
            // WI-0096 §11: truly-parallel siblings share the same box_x — no
            // per-row indent stagger (which used to imply sequential steps).
            let box_x = col_x;
            let box_w = this_col_w.max(4);
            let row_y = area.y + row_idx as u16 * 3;
            if row_y + 3 > area.y + area.height {
                break;
            }
            let box_area = Rect::new(box_x, row_y, box_w, 3);

            let (label, style) = match row {
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
                    step_box_label_and_style(&name, &step.status, is_current, box_w)
                }
                ColumnRow::Collapsed { step, extra } => {
                    let name = format!("{} (+{} completed)", step.name, extra);
                    step_box_label_and_style(&name, &step.status, false, box_w)
                }
            };

            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style);
            let para = Paragraph::new(label).block(block).style(style);
            frame.render_widget(para, box_area);

            // Arrow between this column and the next, on the middle row of
            // the FIRST row of boxes only (so it visually connects column
            // headers without overlapping parallel siblings).
            if col_idx + 1 < num_cols && row_idx == 0 {
                let arrow_x = col_x + this_col_w;
                if arrow_x < area.x + area.width {
                    let arrow_area = Rect::new(arrow_x, row_y + 1, 1, 1);
                    frame.render_widget(
                        Paragraph::new("\u{2192}").style(Style::default().fg(Color::DarkGray)),
                        arrow_area,
                    );
                }
            }
        }

        // Overflow indicator in the last visible row when there are hidden
        // steps below the fold. Replaces the last shown box position.
        if hidden > 0 && !rows_to_show.is_empty() {
            let last_row = rows_to_show.len().saturating_sub(1);
            let row_y = area.y + last_row as u16 * 3;
            if row_y + 3 <= area.y + area.height {
                let box_w = this_col_w.max(4);
                let box_area = Rect::new(col_x, row_y, box_w, 3);
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

        col_x += this_col_w + 1;
    }
}

/// A single rendered row in a workflow-strip column (WI-0096 §11).
enum ColumnRow<'a> {
    /// One step's box. `queued` steps (beyond `max_concurrent`) get a `·`
    /// name prefix.
    Step {
        step: &'a WorkflowStepView,
        queued: bool,
    },
    /// A collapsed summary of a parallel group's completed siblings:
    /// `<step_name> (+N completed)`.
    Collapsed {
        step: &'a WorkflowStepView,
        extra: usize,
    },
}

/// Whether a step status counts as "completed" for collapse purposes.
fn is_completed_status(status: &str) -> bool {
    matches!(status, "done" | "cancelled" | "skipped")
}

/// Build the ordered display rows for one strip column.
///
/// - In a **parallel group** (more than one step in the column) two or more
///   completed siblings collapse into a single `<name> (+N completed)` row so
///   large fan-outs stay compact; the user can scroll to reveal steps below.
///   Sequential columns (one step) never collapse — behavior is unchanged.
/// - Active steps beyond `max_concurrent` are marked `queued` (rendered with a
///   `·` prefix). `None` (unlimited) marks nothing.
fn build_column_rows<'a>(
    col: &[&'a WorkflowStepView],
    max_concurrent: Option<usize>,
) -> Vec<ColumnRow<'a>> {
    let is_parallel_group = col.len() > 1;
    let completed: Vec<&WorkflowStepView> = col
        .iter()
        .filter(|s| is_completed_status(&s.status))
        .copied()
        .collect();
    let active: Vec<&WorkflowStepView> = col
        .iter()
        .filter(|s| !is_completed_status(&s.status))
        .copied()
        .collect();

    let mut rows: Vec<ColumnRow> = Vec::new();
    if is_parallel_group && completed.len() >= 2 {
        rows.push(ColumnRow::Collapsed {
            step: completed[0],
            extra: completed.len() - 1,
        });
    } else {
        for s in completed {
            rows.push(ColumnRow::Step {
                step: s,
                queued: false,
            });
        }
    }

    for (i, s) in active.iter().enumerate() {
        let queued = matches!(max_concurrent, Some(mc) if i >= mc) && s.status == "pending";
        rows.push(ColumnRow::Step { step: s, queued });
    }
    rows
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
            name: format!("[setup] {}", ps.description),
            status: phase_step_status_to_str(&ps.status).to_string(),
            agent: None,
            model: None,
            depends_on: Vec::new(),
        });
    }

    for info in &state.steps {
        let status = state
            .step_states
            .get(&info.name)
            .map(step_state_to_str)
            .unwrap_or("pending")
            .to_string();
        steps.push(WorkflowStepView {
            name: info.name.clone(),
            status,
            agent: info.agent.clone(),
            model: info.model.clone(),
            depends_on: info.depends_on.clone(),
        });
    }

    for ps in &state.teardown_step_states {
        steps.push(WorkflowStepView {
            name: format!("[teardown] {}", ps.description),
            status: phase_step_status_to_str(&ps.status).to_string(),
            agent: None,
            model: None,
            depends_on: Vec::new(),
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

fn step_state_to_str(state: &StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running { .. } => "running",
        StepState::Succeeded => "done",
        StepState::Failed { .. } => "error",
        StepState::Cancelled => "cancelled",
        StepState::Skipped => "skipped",
    }
}

fn phase_step_status_to_str(status: &PhaseStepStatus) -> &'static str {
    match status {
        PhaseStepStatus::Pending => "pending",
        PhaseStepStatus::Running => "running",
        PhaseStepStatus::Succeeded => "done",
        PhaseStepStatus::Failed { .. } => "error",
        PhaseStepStatus::Remediating { .. } => "fixing",
    }
}

/// Group steps into columns by topological depth. Steps at the same depth
/// form a parallel group (same column). Depth is the longest path from any
/// root (step with no dependencies) to this step. Steps that share the exact
/// same set of dependencies at the same depth are grouped together — steps
/// that depend on members of the previous parallel group all land in the next
/// column regardless of which specific member they depend on.
fn build_workflow_columns(state: &WorkflowViewState) -> Vec<Vec<&WorkflowStepView>> {
    use std::collections::HashMap;

    let step_names: HashMap<&str, usize> = state
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();

    let mut depths: Vec<usize> = vec![0; state.steps.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (i, step) in state.steps.iter().enumerate() {
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

    let max_depth = depths.iter().copied().max().unwrap_or(0);
    let mut columns: Vec<Vec<&WorkflowStepView>> = Vec::with_capacity(max_depth + 1);
    for d in 0..=max_depth {
        let col: Vec<&WorkflowStepView> = state
            .steps
            .iter()
            .enumerate()
            .filter(|(i, _)| depths[*i] == d)
            .map(|(_, s)| s)
            .collect();
        if !col.is_empty() {
            columns.push(col);
        }
    }
    columns
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
    status: &str,
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
        "pending" => ("\u{25cb}", Style::default().fg(Color::DarkGray)),
        "running" => (
            "\u{25cf}",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        "done" => ("\u{2713}", Style::default().fg(Color::Green)),
        "error" => (
            "\u{2717}",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        "fixing" => (
            "\u{1f527}",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        "cancelled" | "skipped" => ("\u{2298}", Style::default().fg(Color::DarkGray)),
        _ => ("\u{25cb}", Style::default().fg(Color::DarkGray)),
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

    fn step(name: &str, status: &str, deps: Vec<&str>) -> WorkflowStepView {
        WorkflowStepView {
            name: name.into(),
            status: status.into(),
            agent: None,
            model: None,
            depends_on: deps.into_iter().map(|s| s.into()).collect(),
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
    fn workflow_strip_height_is_zero_when_no_steps() {
        let v = view(vec![]);
        assert_eq!(workflow_strip_height(&v), 0);
    }

    #[test]
    fn workflow_strip_height_3_when_sequential() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "running", vec!["a"]),
        ]);
        assert_eq!(workflow_strip_height(&v), 3);
    }

    #[test]
    fn workflow_strip_height_grows_with_parallel_group() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "running", vec![]),
        ]);
        // 3 parallel steps → 3 * 3 = 9 rows.
        assert_eq!(workflow_strip_height(&v), 9);
    }

    #[test]
    fn workflow_strip_height_uses_all_rows_when_unlimited() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "done", vec![]),
            step("d", "done", vec![]),
            step("e", "done", vec![]),
        ]);
        // `None` means unlimited, so all 5 parallel siblings are accounted for.
        assert_eq!(workflow_strip_height(&v), 15);
    }

    // ── step_box_label_and_style ──────────────────────────────────────────────

    #[test]
    fn step_box_label_pending_uses_circle_glyph_and_dark_gray() {
        let (label, style) = step_box_label_and_style("foo", "pending", false, 20);
        assert!(label.contains('\u{25cb}'));
        assert!(label.contains("foo"));
        assert_eq!(style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn step_box_label_running_uses_filled_circle_blue_bold() {
        let (label, style) = step_box_label_and_style("foo", "running", false, 20);
        assert!(label.contains('\u{25cf}'));
        assert_eq!(style.fg, Some(Color::Blue));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_done_uses_check_glyph_green() {
        let (label, style) = step_box_label_and_style("foo", "done", false, 20);
        assert!(label.contains('\u{2713}'));
        assert_eq!(style.fg, Some(Color::Green));
    }

    #[test]
    fn step_box_label_error_uses_cross_glyph_red_bold() {
        let (label, style) = step_box_label_and_style("foo", "error", false, 20);
        assert!(label.contains('\u{2717}'));
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_current_step_adds_bold_on_top_of_status() {
        let (_, style) = step_box_label_and_style("foo", "done", true, 20);
        // Done is not bold by default, but is_current adds BOLD.
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn step_box_label_truncates_long_name() {
        let (label, _) = step_box_label_and_style("very-long-step-name", "pending", false, 12);
        assert!(label.contains('\u{2026}'));
    }

    // ── WI-0096 §11 parallel strip rendering ────────────────────────────────

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
    fn column_rows_collapse_completed_parallel_siblings() {
        // A parallel group of 5 where 3 are completed collapses the completed
        // ones into a single "(+N completed)" summary at the top, followed by
        // the still-active siblings.
        let steps = vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "done", vec![]),
            step("d", "running", vec![]),
            step("e", "pending", vec![]),
        ];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        let rows = build_column_rows(&col, None);

        // First row is the collapsed summary of the 3 completed siblings.
        match &rows[0] {
            ColumnRow::Collapsed { step, extra } => {
                assert_eq!(step.name, "a", "collapse names the first completed sibling");
                assert_eq!(
                    *extra, 2,
                    "3 completed = 1 representative + 2 others → \"(+2 completed)\""
                );
            }
            _ => panic!("expected a collapsed summary row for the completed siblings"),
        }
        // The two active siblings follow as their own rows.
        assert_eq!(rows.len(), 3);
        assert!(matches!(
            &rows[1],
            ColumnRow::Step { step, .. } if step.name == "d"
        ));
        assert!(matches!(
            &rows[2],
            ColumnRow::Step { step, .. } if step.name == "e"
        ));
    }

    #[test]
    fn sequential_column_never_collapses() {
        // A single-step column is not a parallel group, so a lone completed
        // step is rendered as its own box, never collapsed.
        let steps = vec![step("only", "done", vec![])];
        let col: Vec<&WorkflowStepView> = steps.iter().collect();
        let rows = build_column_rows(&col, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], ColumnRow::Step { .. }));
    }

    #[test]
    fn column_rows_mark_steps_beyond_max_concurrent_as_queued() {
        // With max_concurrent = 2, pending siblings past the second are marked
        // queued (rendered with a `·` prefix).
        let steps = vec![
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
}
