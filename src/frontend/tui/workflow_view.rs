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

use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};

/// Compute the rows needed for the workflow strip given a view state.
/// `max_parallel` is capped at 3 (each box is 3 rows tall, so the strip
/// caps at 9 rows). Returns 0 when `state` is empty / has no steps.
pub fn workflow_strip_height(state: &WorkflowViewState) -> u16 {
    if state.steps.is_empty() {
        return 0;
    }
    let columns = build_workflow_columns(state);
    let max_parallel = columns.iter().map(|c| c.len()).max().unwrap_or(1);
    let rows = max_parallel.min(3) as u16;
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

        let steps_to_show: Vec<&WorkflowStepView> = col_steps
            .iter()
            .skip(scroll_offset)
            .take(visible_rows)
            .copied()
            .collect();
        let hidden = col_steps.len().saturating_sub(scroll_offset + visible_rows);

        for (row_idx, step) in steps_to_show.iter().enumerate() {
            // Indent parallel siblings by row index (1 cell per extra row).
            let indent = row_idx as u16;
            let box_x = (col_x + indent).min(area.x + area.width.saturating_sub(4));
            let box_w = this_col_w.saturating_sub(indent).max(4);
            let row_y = area.y + row_idx as u16 * 3;
            if row_y + 3 > area.y + area.height {
                break;
            }
            let box_area = Rect::new(box_x, row_y, box_w, 3);

            let is_current = state
                .current_step
                .as_ref()
                .map(|c| c == &step.name)
                .unwrap_or(false);
            let (label, style) =
                step_box_label_and_style(&step.name, &step.status, is_current, box_w);

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
        // steps. Replaces the last shown step's box position.
        if hidden > 0 && !steps_to_show.is_empty() {
            let last_row = steps_to_show.len().saturating_sub(1);
            let row_y = area.y + last_row as u16 * 3;
            if row_y + 3 <= area.y + area.height {
                let indent = last_row as u16;
                let box_x = (col_x + indent).min(area.x + area.width.saturating_sub(4));
                let box_w = this_col_w.saturating_sub(indent).max(4);
                let box_area = Rect::new(box_x, row_y, box_w, 3);
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
    fn workflow_strip_height_caps_at_three_rows_of_boxes() {
        let v = view(vec![
            step("a", "done", vec![]),
            step("b", "done", vec![]),
            step("c", "done", vec![]),
            step("d", "done", vec![]),
            step("e", "done", vec![]),
        ]);
        // 5 parallel siblings → still capped at 3 box-rows = 9 rows.
        assert_eq!(workflow_strip_height(&v), 9);
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
}
