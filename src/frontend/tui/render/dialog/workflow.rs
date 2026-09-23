//! The Workflow Control Board and its two lighter siblings.
//!
//! Split out of `render/dialog.rs` by WI 0114 F-51: one `render_*` per
//! dialog variant, so the 1,700-line `match` in `render_dialog` is one call
//! per arm.

use super::super::*;
use crate::frontend::tui::dialogs;

pub(crate) fn render_control_board(
    state: &dialogs::WorkflowControlBoardState,
    area: Rect,
    frame: &mut Frame,
) {
    let extra_reasons = [
        state.continue_unavailable_reason.is_some(),
        state.cancel_to_previous_unavailable_reason.is_some(),
        state.finish_workflow_unavailable_reason.is_some(),
        state.restart_unavailable_reason.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count() as u16;
    let failed = !state.failure_lines.is_empty();
    let base_height: u16 = if state.can_finish { 14 } else { 12 };
    // A failure banner adds its detail lines plus a blank separator.
    let failure_height = if failed {
        state.failure_lines.len() as u16 + 2
    } else {
        0
    };
    // Width fits the longest reason line (+ left margin) when present;
    // otherwise the diamond layout's natural minimum is comfortable.
    let max_reason_w = [
        state.continue_unavailable_reason.as_deref(),
        state.cancel_to_previous_unavailable_reason.as_deref(),
        state.finish_workflow_unavailable_reason.as_deref(),
        state.restart_unavailable_reason.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(|s| unicode_width::UnicodeWidthStr::width(s) + 15)
    .max()
    .unwrap_or(0) as u16;
    let max_failure_w = state
        .failure_lines
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()) + 6)
        .max()
        .unwrap_or(0) as u16;
    let step_w = unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16 + 10;
    let width = max_reason_w
        .max(max_failure_w)
        .max(step_w)
        .max(56)
        .min(area.width.saturating_sub(4));
    let dialog_area = dialogs::centered_fixed(
        width,
        (base_height + extra_reasons + failure_height).min(area.height.saturating_sub(2)),
        area,
    );
    let (title, frame_colour) = if failed {
        ("Workflow Control — step failed", Color::Red)
    } else if state.can_dismiss {
        ("Workflow Control (step running)", Color::Yellow)
    } else {
        ("Workflow Control", Color::Yellow)
    };
    let inner = dialogs::render_dialog_frame(title, frame_colour, dialog_area, frame);

    let arrow_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::White);
    let dimmed_style = Style::default().fg(Color::DarkGray);
    let step_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let (right_arrow_style, right_label_style) = if state.can_launch_next {
        (arrow_style, label_style)
    } else {
        (dimmed_style, dimmed_style)
    };
    let (down_arrow_style, down_label_style) = if state.can_continue_current {
        (arrow_style, label_style)
    } else {
        (dimmed_style, dimmed_style)
    };
    let (left_arrow_style, left_label_style) = if state.can_go_back {
        (arrow_style, label_style)
    } else {
        (dimmed_style, dimmed_style)
    };
    // Restart is disabled in a parallel group unless this is the
    // focused container (WI-0096 §10).
    let restart_disabled = state.restart_unavailable_reason.is_some() || !state.can_restart;
    let (up_arrow_style, up_label_style) = if restart_disabled {
        (dimmed_style, dimmed_style)
    } else {
        (arrow_style, label_style)
    };

    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::raw(if failed { " Failed step: " } else { " Step: " }),
        Span::styled(&state.step_name, step_style),
    ])];
    if failed {
        let err_style = Style::default().fg(Color::Red);
        for line in &state.failure_lines {
            lines.push(Line::from(Span::styled(format!("   {line}"), err_style)));
        }
    }
    lines.push(Line::from(""));
    // ↑ Restart (top of diamond)
    lines.push(Line::from(vec![
        Span::raw("         "),
        Span::styled("\u{2191}", up_arrow_style),
        Span::styled(
            if failed {
                " Restart failed step"
            } else {
                " Restart current step"
            },
            up_label_style,
        ),
    ]));
    if let Some(ref reason) = state.restart_unavailable_reason {
        lines.push(Line::from(Span::styled(
            format!("           {reason}"),
            dimmed_style,
        )));
    }
    lines.push(Line::from(""));
    lines.extend([
        // ← Cancel to prev    → Next: new container
        Line::from(vec![
            Span::styled("\u{2190}", left_arrow_style),
            Span::styled(" Cancel to prev", left_label_style),
            Span::raw("   "),
            Span::styled("\u{2192}", right_arrow_style),
            Span::styled(
                format!(
                    " {}",
                    state
                        .launch_next_label
                        .as_deref()
                        .unwrap_or("Next: new container")
                ),
                right_label_style,
            ),
        ]),
        Line::from(""),
        // ↓ Next: same container (bottom of diamond)
        Line::from(vec![
            Span::raw("         "),
            Span::styled("\u{2193}", down_arrow_style),
            Span::styled(" Next: same container", down_label_style),
        ]),
    ]);
    if let Some(ref reason) = state.continue_unavailable_reason {
        lines.push(Line::from(Span::styled(
            format!("           {reason}"),
            dimmed_style,
        )));
    } else {
        lines.push(Line::from(""));
    }
    if state.can_finish {
        lines.push(Line::from(""));
        let finish_style = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[Enter]", finish_style),
            Span::styled(" Finish workflow", finish_style),
        ]));
    }
    lines.push(Line::from(""));
    if state.can_dismiss {
        lines.push(Line::from(Span::styled(
            "  [^C] Abort   [p] Pause   [Esc] Dismiss",
            dimmed_style,
        )));
    } else if failed {
        lines.push(Line::from(Span::styled(
            "  [^C] Cancel workflow   [Esc] Pause",
            dimmed_style,
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  [^C] Abort   [Esc] Pause",
            dimmed_style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn render_yolo_countdown(
    state: &dialogs::WorkflowYoloCountdownState,
    area: Rect,
    frame: &mut Frame,
) {
    let emoji = if state.remaining_secs.is_multiple_of(2) {
        "\u{26a0}\u{fe0f}"
    } else {
        "\u{1f918}"
    };
    let title = format!("{} Yolo in {}s", emoji, state.remaining_secs);
    let step_w = unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16;
    let width = step_w
        .saturating_add(20)
        .max(56)
        .min(area.width.saturating_sub(4));
    let dialog_area = dialogs::centered_fixed(width, 9, area);
    let inner = dialogs::render_dialog_frame(&title, Color::Magenta, dialog_area, frame);
    let text = format!(
        "  Step: {}\n  Auto-advancing in {}s\n\n  [Esc] Cancel   [Ctrl-W] Control board",
        state.step_name, state.remaining_secs
    );
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

pub(crate) fn render_step_confirm(
    state: &dialogs::WorkflowStepConfirmState,
    area: Rect,
    frame: &mut Frame,
) {
    let body_w = unicode_width::UnicodeWidthStr::width(
        format!(
            "  Step '{}' done. Advance to '{}'?",
            state.completed_step, state.next_step
        )
        .as_str(),
    ) as u16
        + 4;
    let width = body_w.max(64).min(area.width.saturating_sub(4));
    let dialog_area = dialogs::centered_fixed(width, 8, area);
    let inner = dialogs::render_dialog_frame("Step Complete", Color::Green, dialog_area, frame);
    let lines = vec![
        Line::from(format!(
            "  Step '{}' done. Advance to '{}'?",
            state.completed_step, state.next_step
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  [Enter] yes   [Esc] pause   [Ctrl+W] full control board",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
