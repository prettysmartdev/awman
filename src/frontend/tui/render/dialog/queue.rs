//! The `Ctrl-Q` work-queue panel, the item body view and the close-tab
//! prompt for a tab that holds a lease (WI 0143). Pure presentation over
//! [`QueuePanelState`]; nothing here decides anything.

use super::super::*;
use crate::frontend::tui::dialogs;
use crate::frontend::tui::queue::{
    format_age, format_labels, LeaseCloseState, QueueEntry, QueuePanelState,
};

fn trunc(text: &str, max: usize) -> String {
    super::super::command_box::truncate_middle(text, max)
}

pub(crate) fn render_queue_panel(state: &QueuePanelState, area: Rect, frame: &mut Frame) {
    if state.showing_body {
        if let Some(entry) = state.selected_entry() {
            render_body(entry, state.message.as_deref(), area, frame);
            return;
        }
    }
    let width = area.width.saturating_sub(4).clamp(40, 120);
    let height = ((state.rows.len() as u16).saturating_add(8))
        .max(10)
        .min(area.height.saturating_sub(2));
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let inner = dialogs::render_dialog_frame("Work queue", Color::Cyan, dialog_area, frame);
    let w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    if let Some(error) = &state.error {
        lines.push(Line::from(Span::styled(
            trunc(&format!("! {error}"), w),
            Style::default().fg(Color::Red),
        )));
    }
    if let Some(message) = &state.message {
        lines.push(Line::from(Span::styled(
            trunc(message, w),
            Style::default().fg(Color::Yellow),
        )));
    }
    if state.rows.is_empty() {
        let text = if state.loaded {
            "No eligible work items."
        } else {
            "Loading the queue\u{2026}"
        };
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Fixed columns: server, priority, age, for-you; title and labels share the rest.
        let (server_w, pri_w, age_w, you_w) = (12usize, 4usize, 5usize, 8usize);
        let rest = w
            .saturating_sub(server_w + pri_w + age_w + you_w + 5)
            .max(10);
        let title_w = rest * 55 / 100;
        let labels_w = rest - title_w;
        lines.push(Line::from(Span::styled(
            format!(
                "{:<title_w$} {:<server_w$} {:<labels_w$} {:>pri_w$} {:>age_w$} {:<you_w$}",
                "TITLE", "SERVER", "LABELS", "PRI", "AGE", "FOR YOU"
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        // Keep the selected row on screen.
        let visible = (inner.height as usize)
            .saturating_sub(lines.len() + 2)
            .max(1);
        let first = state.selected.saturating_sub(visible - 1);
        for (idx, row) in state.rows.iter().enumerate().skip(first).take(visible) {
            let you = if row.for_you { "for you" } else { "" };
            let text = format!(
                "{:<title_w$} {:<server_w$} {:<labels_w$} {:>pri_w$} {:>age_w$} {:<you_w$}",
                trunc(&row.title, title_w),
                trunc(&row.server, server_w),
                trunc(&format_labels(&row.labels), labels_w),
                row.priority,
                format_age(row.age_ms),
                you,
            );
            let style = if idx == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if row.for_you {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }
    let hint = "[\u{2191}/\u{2193}] move  [Enter] details  [p] pick  [r] refresh  [Esc] close";
    let used = lines.len() as u16;
    frame.render_widget(Paragraph::new(lines), inner);
    if inner.height > used + 1 {
        let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            hint_area,
        );
    }
}

fn render_body(entry: &QueueEntry, message: Option<&str>, area: Rect, frame: &mut Frame) {
    let width = area.width.saturating_sub(4).clamp(40, 100);
    let height = area.height.saturating_sub(4).clamp(10, 24);
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let inner = dialogs::render_dialog_frame(&entry.title, Color::Cyan, dialog_area, frame);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("server    ", dim),
            Span::raw(entry.server.clone()),
        ]),
        Line::from(vec![
            Span::styled("kind      ", dim),
            Span::raw(entry.kind.clone()),
        ]),
        Line::from(vec![
            Span::styled("priority  ", dim),
            Span::raw(entry.priority.to_string()),
            Span::styled("   age ", dim),
            Span::raw(format_age(entry.age_ms)),
            Span::styled(if entry.for_you { "   for you" } else { "" }, dim),
        ]),
        Line::from(vec![
            Span::styled("labels    ", dim),
            Span::raw(format_labels(&entry.labels)),
        ]),
        Line::from(""),
    ];
    if let Some(summary) = &entry.summary {
        lines.push(Line::from(summary.clone()));
        lines.push(Line::from(""));
    }
    match &entry.body {
        Some(body) => lines.extend(body.lines().map(|l| Line::from(l.to_string()))),
        None => lines.push(Line::from(Span::styled(
            "The full body arrives with the claim.",
            dim,
        ))),
    }
    if let Some(message) = message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    if inner.height > 1 {
        let hint_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Span::styled("[p] pick  [Esc] back to the list", dim)),
            hint_area,
        );
    }
}

pub(crate) fn render_lease_close_confirm(state: &LeaseCloseState, area: Rect, frame: &mut Frame) {
    let width = area.width.saturating_sub(6).clamp(40, 70);
    let dialog_area = dialogs::centered_fixed(width, 12, area);
    let inner =
        dialogs::render_dialog_frame("Close work item tab?", Color::Yellow, dialog_area, frame);
    let text = format!(
        "\"{}\" ({}) is still running.\n\nWhat should happen to the item?\n\n  [r] Release it back to the queue\n  [c] Cancel it\n  [Esc] Keep it running",
        trunc(&state.lease.title, 40),
        state.lease.server
    );
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}
