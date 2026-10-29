//! The review dialog: a keyed question whose body scrolls under a footer
//! that is always on screen.
//!
//! The footer holds the scroll position (when there is more than fits), the
//! pinned note, one row per choice and the Esc hint, so however long the body
//! or small the terminal, the choices are never pushed out of the frame. The
//! body is one of three shapes (`ReviewBody`): exact text (an argv under
//! review), word-wrapped prose, or the workflow prerequisite report.

use super::super::*;
use crate::data::workflow_prerequisites::{
    PrerequisiteCheck, PrerequisiteStatus, WorkflowPrerequisiteReport,
};
use crate::frontend::tui::dialogs::{self, ReviewBody, ReviewState};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Widest a review grows: past this, lines get too long to read.
const MAX_TEXT_WIDTH: usize = 100;
/// Rows the frame takes: two borders plus one row of padding on each side.
const FRAME_ROWS: u16 = 4;
/// Columns the frame takes: two borders plus one column of padding each side.
const FRAME_COLS: u16 = 4;
/// Below this many body rows, the note moves from the footer to the body's
/// end, so it never squeezes the body out of sight.
const MIN_BODY_ROWS: u16 = 4;

/// Hard-wrap `line` at `width` display columns, keeping every character
/// (spaces included) so a reviewed argv reads exactly as it will run.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_w = 0;
    for ch in line.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + ch_w > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(ch);
        current_w += ch_w;
    }
    out.push(current);
    out
}

/// Word-wrap `text` to `width` columns. The first row starts with `first`,
/// every later row with `rest` (a hanging indent). A word wider than a row
/// is hard-wrapped rather than overflowing.
pub(super) fn wrap_words(text: &str, width: usize, first: &str, rest: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = first.to_string();
    let mut prefix_w = first.width();
    let mut current_w = prefix_w;
    for word in text.split_whitespace() {
        let word_w = word.width();
        let gap = usize::from(current_w > prefix_w);
        if current_w + gap + word_w <= width {
            if gap == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_w += gap + word_w;
            continue;
        }
        if current_w > prefix_w {
            rows.push(std::mem::replace(&mut current, rest.to_string()));
            prefix_w = rest.width();
            current_w = prefix_w;
        }
        let room = width.saturating_sub(prefix_w).max(1);
        let mut pieces = hard_wrap(word, room).into_iter().peekable();
        while let Some(piece) = pieces.next() {
            current_w += piece.width();
            current.push_str(&piece);
            if pieces.peek().is_some() {
                rows.push(std::mem::replace(&mut current, rest.to_string()));
                prefix_w = rest.width();
                current_w = prefix_w;
            }
        }
    }
    rows.push(current);
    rows
}

/// Word-wrap every line of `text`, each keeping its own leading indentation
/// (so indented JSON still lines up); blank lines stay blank.
pub(super) fn prose_rows(text: &str, width: usize) -> Vec<String> {
    text.lines()
        .flat_map(|line| {
            let body = line.trim_start();
            let indent = &line[..line.len() - body.len()];
            wrap_words(body, width, indent, indent)
        })
        .collect()
}

fn natural_width(text: &str) -> usize {
    text.lines().map(UnicodeWidthStr::width).max().unwrap_or(0)
}

/// How a check's status is drawn: a mark and a colour.
fn status_mark(status: PrerequisiteStatus) -> (&'static str, Color) {
    match status {
        PrerequisiteStatus::Satisfied => ("✓", Color::Green),
        PrerequisiteStatus::NeedsConsent => ("!", Color::Yellow),
        PrerequisiteStatus::Missing => ("✗", Color::Red),
        PrerequisiteStatus::Unknown => ("?", Color::Magenta),
    }
}

/// One check: a mark, its name and its status on the first row; the
/// explanation and the steps that need it wrapped and indented under it.
fn check_lines(check: &PrerequisiteCheck, width: usize) -> Vec<Line<'static>> {
    let (mark, color) = status_mark(check.status);
    let status = check.status.label();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // Mark + space before the name; two spaces between it and the status.
    let name_w = width.saturating_sub(2 + 2 + status.width()).max(10);
    let names = wrap_words(&check.prerequisite.label(), name_w, "", "");
    let mut lines = Vec::new();
    for (i, name) in names.into_iter().enumerate() {
        if i == 0 {
            let pad = width.saturating_sub(2 + name.width() + status.width());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{mark} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(name, bold),
                Span::raw(" ".repeat(pad.max(2))),
                Span::styled(status, Style::default().fg(color)),
            ]));
        } else {
            lines.push(Line::from(Span::styled(format!("  {name}"), bold)));
        }
    }
    // A satisfied check needs no reading: its detail is de-emphasised.
    let detail = if check.status == PrerequisiteStatus::Satisfied {
        Style::default().fg(Color::Gray)
    } else {
        Style::default()
    };
    for row in wrap_words(&check.explanation, width, "    ", "    ") {
        lines.push(Line::from(Span::styled(row, detail)));
    }
    // A dynamic run's steps are not known yet; the note says so once
    // rather than every check saying "any teardown step".
    if !check.needed_by.is_empty() {
        let needed_by = format!("needed by: {}", check.needed_by_label());
        for row in wrap_words(&needed_by, width, "    ", "      ") {
            lines.push(Line::from(Span::styled(
                row,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines
}

/// A heading and its bullets, word-wrapped with a hanging indent.
fn bullet_section(heading: &str, items: &[String], width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            heading.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    for item in items {
        lines.extend(
            wrap_words(item, width, "  • ", "    ")
                .into_iter()
                .map(Line::from),
        );
    }
    lines
}

/// The report as one entry per check, the ones that need attention first,
/// then its advisories and notes. The summary line is the dialog's title.
fn report_lines(report: &WorkflowPrerequisiteReport, width: usize) -> Vec<Line<'static>> {
    let mut checks: Vec<&PrerequisiteCheck> = report.checks.iter().collect();
    checks.sort_by_key(|check| match check.status {
        PrerequisiteStatus::Missing => 0,
        PrerequisiteStatus::NeedsConsent => 1,
        PrerequisiteStatus::Unknown => 2,
        PrerequisiteStatus::Satisfied => 3,
    });
    let mut lines = Vec::new();
    for (i, check) in checks.into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(check_lines(check, width));
    }
    if !report.advisories.is_empty() {
        let items: Vec<String> = report
            .advisories
            .iter()
            .map(|a| format!("{}: {}", a.step.kind.label(), a.message))
            .collect();
        lines.extend(bullet_section("Advisories", &items, width));
    }
    if !report.notes.is_empty() {
        lines.extend(bullet_section("Notes", &report.notes, width));
    }
    lines
}

/// The review dialog. Sized to its content, up to the terminal; the body
/// scrolls under the footer. A gated choice is shown as unavailable until
/// the end of the body has been on screen.
pub(crate) fn render_review(state: &ReviewState, area: Rect, frame: &mut Frame) {
    let gated = !state.gated_keys.is_empty();
    let natural = match &state.body {
        ReviewBody::Exact(text) | ReviewBody::Prose(text) => natural_width(text),
        ReviewBody::Prerequisites(_) => MAX_TEXT_WIDTH,
    }
    .max(natural_width(&state.note).min(80))
    .min(MAX_TEXT_WIDTH) as u16;
    let title_w = state.title.width() as u16 + 4;
    let width = natural
        .saturating_add(FRAME_COLS + 2)
        .max(title_w)
        .max(60)
        .min(area.width.saturating_sub(4));
    let text_w = width.saturating_sub(FRAME_COLS).max(1) as usize;
    let dim = Style::default().fg(Color::DarkGray);

    // A title too long for the border is shortened there and given in full
    // at the top of the body.
    let mut body: Vec<Line<'static>> = Vec::new();
    let border_title = if title_w > width {
        for row in wrap_words(&state.title, text_w, "", "") {
            body.push(Line::from(Span::styled(
                row,
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
        body.push(Line::from(""));
        let room = width.saturating_sub(6) as usize;
        let mut short: String = String::new();
        for ch in state.title.chars() {
            if short.width() + ch.width().unwrap_or(0) + 1 > room {
                break;
            }
            short.push(ch);
        }
        format!("{}…", short.trim_end())
    } else {
        state.title.clone()
    };
    match &state.body {
        ReviewBody::Exact(text) => body.extend(
            text.lines()
                .flat_map(|line| hard_wrap(line, text_w))
                .map(Line::from),
        ),
        ReviewBody::Prose(text) => {
            body.extend(prose_rows(text, text_w).into_iter().map(Line::from))
        }
        ReviewBody::Prerequisites(report) => body.extend(report_lines(report, text_w)),
    }
    while body.last().is_some_and(|line| line.width() == 0) {
        body.pop();
    }

    let note: Vec<Line<'static>> = if state.note.trim().is_empty() {
        Vec::new()
    } else {
        prose_rows(&state.note, text_w)
            .into_iter()
            .map(|row| Line::from(Span::styled(row, Style::default().fg(Color::Gray))))
            .collect()
    };
    let mut keys: Vec<Line<'static>> = Vec::new();
    for (ch, label) in &state.keys {
        let (text, style) = if state.accepts(*ch) {
            (label.clone(), Style::default())
        } else {
            (format!("{label} (scroll to the end first)"), dim)
        };
        let first = format!("[{ch}] ");
        let rest = " ".repeat(first.width());
        for row in wrap_words(&text, text_w, &first, &rest) {
            keys.push(Line::from(Span::styled(row, style)));
        }
    }
    let dismiss = state.dismiss_label.as_deref().unwrap_or("cancel");
    keys.push(Line::from(Span::styled(format!("[Esc] {dismiss}"), dim)));

    // Lay out: the note sits in the footer (after a rule) unless that would
    // leave the body fewer than MIN_BODY_ROWS rows, in which case it is the
    // end of the body. The status row is there when the body scrolls, and
    // always for a gated review.
    let max_inner = area.height.saturating_sub(2 + FRAME_ROWS);
    let note_rows = if note.is_empty() {
        0
    } else {
        note.len() as u16 + 2
    };
    let key_rows = keys.len() as u16;
    let note_in_footer = !note.is_empty()
        && !body.is_empty()
        && max_inner.saturating_sub(key_rows + note_rows + 1) >= MIN_BODY_ROWS;
    if !note.is_empty() && !note_in_footer {
        if !body.is_empty() {
            body.push(Line::from(""));
        }
        body.extend(note.iter().cloned());
    }
    let mut footer_rows = key_rows + if note_in_footer { note_rows } else { 1 };
    let fits = body.len() as u16 + footer_rows <= max_inner;
    let show_status = gated || !fits;
    if show_status {
        footer_rows += 1;
    }
    let inner_h = (body.len() as u16)
        .saturating_add(footer_rows)
        .min(max_inner);
    let dialog_area = dialogs::centered_fixed(width, inner_h + FRAME_ROWS, area);
    let inner = dialogs::render_dialog_frame(&border_title, Color::Yellow, dialog_area, frame);
    let footer_h = footer_rows.min(inner.height);
    let body_area = Rect {
        height: inner.height - footer_h,
        ..inner
    };
    let footer_area = Rect {
        y: inner.y + body_area.height,
        height: footer_h,
        ..inner
    };

    // With no room for the body at all, nothing can be reviewed: the end is
    // never "seen", so a gated choice stays unavailable.
    let max_scroll = if body_area.height == 0 && !body.is_empty() {
        usize::MAX
    } else {
        body.len().saturating_sub(body_area.height as usize)
    };
    state.max_scroll.set(max_scroll);
    let scroll = state.scroll.min(max_scroll).min(body.len());
    let visible: Vec<Line> = body
        .iter()
        .skip(scroll)
        .take(body_area.height as usize)
        .cloned()
        .collect();
    frame.render_widget(Paragraph::new(visible), body_area);

    let mut footer = Vec::new();
    if show_status {
        let shown_to = (scroll + body_area.height as usize).min(body.len());
        footer.push(Line::from(if body_area.height == 0 {
            Span::styled(
                "⚠ terminal too small to show the review",
                Style::default().fg(Color::Red),
            )
        } else if state.fully_seen() {
            Span::styled(
                format!(
                    "end of review — lines {}–{shown_to} of {}",
                    scroll + 1,
                    body.len()
                ),
                dim,
            )
        } else {
            Span::styled(
                format!(
                    "▼ more below — ↑/↓/PgDn to scroll (lines {}–{shown_to} of {})",
                    scroll + 1,
                    body.len()
                ),
                Style::default().fg(Color::Yellow),
            )
        }));
    }
    if note_in_footer {
        footer.push(Line::from(Span::styled("─".repeat(text_w), dim)));
        footer.extend(note);
    }
    footer.push(Line::from(""));
    footer.extend(keys);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_words_breaks_at_spaces_with_a_hanging_indent() {
        let rows = wrap_words("alpha beta gamma delta", 12, "  • ", "    ");
        assert_eq!(
            rows,
            vec!["  • alpha", "    beta", "    gamma", "    delta"]
        );
        for row in &rows {
            assert!(row.width() <= 12, "{row:?}");
        }
    }

    #[test]
    fn wrap_words_hard_wraps_a_word_wider_than_the_row() {
        let rows = wrap_words("x abcdefghijkl", 6, "", "");
        assert_eq!(rows, vec!["x", "abcdef", "ghijkl"]);
    }

    #[test]
    fn prose_rows_keep_each_lines_indentation_and_blank_lines() {
        let rows = prose_rows("{\n  \"a\": 1\n}\n\nsome words here", 8);
        assert_eq!(
            rows,
            vec!["{", "  \"a\": 1", "}", "", "some", "words", "here"]
        );
    }
}
