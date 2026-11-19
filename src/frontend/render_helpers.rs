//! Rendering shared by more than one frontend.
//!
//! Layer 3, and only Layer 3. The ready summary box is shared by the CLI and
//! TUI so they present the same ordered rows and diagnostics.

use crate::data::step_status::StepStatus;
use unicode_width::UnicodeWidthStr;

const FALLBACK_WIDTH: usize = 80;
const MAX_BOX_WIDTH: usize = 120;
const MAX_DIAGNOSTIC_CHARS: usize = 16 * 1024;
const MIN_BOX_WIDTH: usize = 30;

/// The one-character status marker a summary row is drawn with.
pub fn step_glyph(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "-",
        StepStatus::Running => "\u{2026}",
        StepStatus::Done => "\u{2713}",
        StepStatus::Skipped => "\u{2013}",
        StepStatus::Warn(_) => "\u{26a0}",
        StepStatus::Failed(_) => "\u{2717}",
    }
}

/// Returns a useful maximum width for text sent to stderr. Redirected output
/// uses a predictable width so it remains readable in logs and pipes.
pub fn stderr_render_width() -> usize {
    use std::io::IsTerminal;

    if !std::io::stderr().is_terminal() {
        return FALLBACK_WIDTH;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(cols, _)| usize::from(cols))
        .filter(|width| *width > 0)
        .unwrap_or(FALLBACK_WIDTH)
}

/// Conservative width for ready summaries in the TUI execution pane. The
/// pane can lose a quarter of the frame to the git sidebar and two columns
/// to its border.
pub(crate) fn tui_ready_render_width(terminal_width: usize) -> usize {
    terminal_width
        .saturating_sub(terminal_width / 4)
        .saturating_sub(2)
}

/// A titled two-column box of `label → status` rows, sized to its content and
/// capped at `max_width` terminal columns.
pub fn render_summary_box(title: &str, rows: &[(&str, &StepStatus)]) -> String {
    render_summary_box_with_width(title, rows, FALLBACK_WIDTH)
}

/// Width-aware variant of [`render_summary_box`].
pub fn render_summary_box_with_width(
    title: &str,
    rows: &[(&str, &StepStatus)],
    max_width: usize,
) -> String {
    let values: Vec<_> = rows
        .iter()
        .map(|(label, status)| (*label, short_status(status), diagnostic(status)))
        .collect();
    render_summary_values(title, &values, max_width, None)
}

fn short_status(status: &StepStatus) -> String {
    match status {
        StepStatus::Pending => "pending".into(),
        StepStatus::Running => "running".into(),
        StepStatus::Done => "done".into(),
        StepStatus::Skipped => "skipped".into(),
        StepStatus::Warn(_) => "warn".into(),
        StepStatus::Failed(_) => "failed".into(),
    }
}

fn diagnostic(status: &StepStatus) -> Option<&str> {
    match status {
        StepStatus::Warn(reason) | StepStatus::Failed(reason) if !reason.is_empty() => Some(reason),
        _ => None,
    }
}

/// Sanitize bounded text used by ready status events that do not use the box
/// renderer (for example API consumers).
pub(crate) fn sanitize_ready_text(text: &str) -> String {
    sanitize(text, MAX_DIAGNOSTIC_CHARS)
}

/// Untrusted multi-line text (a relayed dialog body) made safe for a
/// terminal: escape sequences are removed, other control and format
/// characters become spaces, and line breaks are kept. Not truncated.
pub(crate) fn terminal_safe_text(text: &str) -> String {
    sanitize(text, usize::MAX)
}

/// [`terminal_safe_text`] for one line (a path, tag or note interpolated
/// into a prompt): line breaks become spaces too, so a value cannot start a
/// line that looks like awman's own output.
pub(crate) fn terminal_safe_line(text: &str) -> String {
    terminal_safe_text(text).replace('\n', " ")
}

/// Preserve every stage and status, showing identical diagnostics only under
/// their most specific (last) row. Machine output keeps the original strings.
pub(crate) fn ready_rows_for_display(
    summary: &crate::data::ready_summary::ReadySummary,
) -> Vec<(String, StepStatus)> {
    let mut rows = summary.rows();
    let mut seen = std::collections::HashSet::new();
    for (_, status) in rows.iter_mut().rev() {
        if let StepStatus::Failed(reason) | StepStatus::Warn(reason) = status {
            if !seen.insert(reason.clone()) {
                reason.clear();
            }
        }
    }
    rows
}

/// Render the shared ready summary at a maximum terminal display width.
///
/// The default is an 80-column fallback for callers without viewport data.
pub fn render_ready_summary(summary: &crate::data::ready_summary::ReadySummary) -> String {
    render_ready_summary_with_width(summary, FALLBACK_WIDTH)
}

/// Width-aware variant of [`render_ready_summary`]. The width is measured in
/// terminal columns, not Unicode scalar values.
pub fn render_ready_summary_with_width(
    summary: &crate::data::ready_summary::ReadySummary,
    max_width: usize,
) -> String {
    let rows = ready_rows_for_display(summary);
    let values: Vec<_> = rows
        .iter()
        .map(|(label, status)| (label.as_str(), short_status(status), diagnostic(status)))
        .collect();
    render_summary_values(
        &format!("Ready Summary ({})", summary.runtime_name),
        &values,
        max_width,
        summary.hint().as_deref(),
    )
}

/// `rows` carry a short status and an optional full reason. Reasons are
/// rendered under their row, inside the box, so the stage label remains
/// visible once and the reason never widens a status cell.
fn render_summary_values(
    title: &str,
    rows: &[(&str, String, Option<&str>)],
    max_width: usize,
    tip: Option<&str>,
) -> String {
    if max_width < MIN_BOX_WIDTH {
        return render_compact_summary(rows, tip, max_width);
    }
    let width = max_width.min(MAX_BOX_WIDTH);
    let outer_width = width;
    // Outer border + cell padding/separators consume seven columns.
    let status_width = rows
        .iter()
        .map(|(_, status, _)| status.width())
        .max()
        .unwrap_or(7)
        .max(7)
        .min(outer_width.saturating_sub(8).max(1));
    let label_width = outer_width.saturating_sub(status_width + 7).max(1);
    let inside_width = outer_width.saturating_sub(2);

    let clean_title = sanitize(title, MAX_DIAGNOSTIC_CHARS);
    let title_width = inside_width.saturating_sub(2).max(1);
    let title_line = truncate_display(&clean_title.replace(['\n', '\r'], " "), title_width);
    let mut out = String::new();
    out.push('┌');
    out.push_str(&"─".repeat(inside_width));
    out.push_str("┐\n");
    let title_pad = inside_width.saturating_sub(title_line.width() + 2);
    out.push_str(&format!("│ {title_line}{} │\n", " ".repeat(title_pad)));
    out.push('├');
    out.push_str(&"─".repeat(label_width + 2));
    out.push('┬');
    out.push_str(&"─".repeat(status_width + 2));
    out.push_str("┤\n");

    for (label, status, reason) in rows {
        let clean_label = sanitize(label, MAX_DIAGNOSTIC_CHARS).replace(['\n', '\r'], " ");
        let clean_label = if clean_label.trim().is_empty() {
            "-"
        } else {
            &clean_label
        };
        let label_lines = wrap_text(clean_label, label_width);
        for (index, text) in label_lines.iter().enumerate() {
            let status_text = if index == 0 { status.as_str() } else { "" };
            let label_pad = label_width.saturating_sub(text.width());
            let status_pad = status_width.saturating_sub(status_text.width());
            out.push_str(&format!(
                "│ {text}{} │ {status_text}{} │\n",
                " ".repeat(label_pad),
                " ".repeat(status_pad)
            ));
        }
        if let Some(reason) = reason {
            let cleaned = sanitize(reason, MAX_DIAGNOSTIC_CHARS);
            let detail_width = outer_width.saturating_sub(5).max(1);
            for detail in wrap_text(&cleaned, detail_width) {
                let pad = outer_width.saturating_sub(detail.width() + 5);
                out.push_str(&format!("│  {detail}{} │\n", " ".repeat(pad)));
            }
        }
    }
    out.push('└');
    out.push_str(&"─".repeat(label_width + 2));
    out.push('┴');
    out.push_str(&"─".repeat(status_width + 2));
    out.push_str("┘\n");

    if let Some(tip) = tip.filter(|tip| !tip.is_empty()) {
        let clean_tip = sanitize(tip, MAX_DIAGNOSTIC_CHARS);
        let tip_width = width.saturating_sub("Tip: ".width()).max(1);
        let lines = wrap_text(&clean_tip, tip_width);
        if let Some((first, rest)) = lines.split_first() {
            out.push_str(&format!("Tip: {first}\n"));
            for line in rest {
                out.push_str(&format!("     {line}\n"));
            }
        }
    }
    out
}

/// Strip terminal escape sequences and replace remaining control characters
/// with spaces. Newlines are retained as detail separators.
fn sanitize(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars));
    let mut chars = text.chars().peekable();
    let mut count = 0;
    while let Some(ch) = chars.next() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        if ch == '\u{1b}' || ch == '\u{009b}' || ch == '\u{009d}' {
            strip_escape_sequence(ch, &mut chars);
            continue;
        }
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
            count += 1;
        } else if ch == '\n' {
            out.push('\n');
            count += 1;
        } else if is_format_or_line_separator(ch) || ch.is_control() {
            out.push(' ');
            count += 1;
        } else {
            out.push(ch);
            count += 1;
        }
    }
    out
}

fn is_format_or_line_separator(ch: char) -> bool {
    matches!(ch as u32,
        0x00ad | 0x0600..=0x0605 | 0x061c | 0x06dd | 0x070f | 0x0890..=0x0891 |
        0x08e2 | 0x180e | 0x200b..=0x200f | 0x2028..=0x202e | 0x2060..=0x206f |
        0xfeff | 0xfff9..=0xfffb | 0x110bd | 0x110cd | 0x13430..=0x1343f |
        0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f)
}

fn strip_escape_sequence<I>(start: char, chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    let kind = if start == '\u{1b}' {
        match chars.next() {
            Some('[') => 'c',
            Some(']') => 'o',
            Some('P' | '_' | '^') => 's',
            Some(ch @ '\u{20}'..='\u{2f}') => {
                let _ = ch;
                while chars
                    .peek()
                    .is_some_and(|ch| ('\u{20}'..='\u{2f}').contains(ch))
                {
                    chars.next();
                }
                if chars
                    .peek()
                    .is_some_and(|ch| ('\u{30}'..='\u{7e}').contains(ch))
                {
                    chars.next();
                }
                return;
            }
            Some('\u{30}'..='\u{7e}') | None => return,
            Some(_) => return,
        }
    } else if start == '\u{009b}' {
        'c'
    } else {
        'o'
    };
    if kind == 'c' {
        // Consume CSI only while bytes follow the grammar: parameters, then
        // intermediates, then one final byte. Leave malformed text untouched.
        while chars
            .peek()
            .is_some_and(|ch| ('\u{30}'..='\u{3f}').contains(ch))
        {
            chars.next();
        }
        while chars
            .peek()
            .is_some_and(|ch| ('\u{20}'..='\u{2f}').contains(ch))
        {
            chars.next();
        }
        if chars.peek().is_some_and(|ch| ('@'..='~').contains(ch)) {
            chars.next();
        }
    } else if kind == 'o' {
        let mut previous_escape = false;
        for ch in chars.by_ref() {
            if ch == '\n' || ch == '\r' {
                break;
            }
            if ch == '\u{7}' || ch == '\u{009c}' || (previous_escape && ch == '\\') {
                break;
            }
            previous_escape = ch == '\u{1b}';
        }
    } else if kind == 's' {
        // DCS, APC and PM payloads terminate with ST. Bound malformed input at
        // the next newline so an escape cannot hide subsequent diagnostics.
        let mut previous_escape = false;
        for ch in chars.by_ref() {
            if ch == '\n' || ch == '\r' || (previous_escape && ch == '\\') || ch == '\u{009c}' {
                break;
            }
            previous_escape = ch == '\u{1b}';
        }
    }
}

/// Wrap text at display-column boundaries. Unbreakable paths are split at
/// Unicode character boundaries, preserving wide characters such as CJK.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut result = Vec::new();
    let paragraphs: Vec<_> = text
        .split('\n')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    for paragraph in paragraphs {
        let mut line = String::new();
        let mut line_width = 0;
        for word in paragraph.split_whitespace() {
            let word_width = scalar_display_width(word);
            if line_width > 0 && line_width + 1 + word_width <= width {
                line.push(' ');
                line.push_str(word);
                line_width += 1 + word_width;
                continue;
            }
            if line_width > 0 {
                result.push(std::mem::take(&mut line));
                line_width = 0;
            }
            let mut piece = String::new();
            let mut piece_width = 0;
            for ch in word.chars() {
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                let ch = if ch_width > width { '…' } else { ch };
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if piece_width > 0 && piece_width + ch_width > width {
                    result.push(std::mem::take(&mut piece));
                    piece_width = 0;
                }
                piece.push(ch);
                piece_width += ch_width;
            }
            if piece_width == width {
                result.push(piece);
            } else {
                line = piece;
                line_width = piece_width;
            }
        }
        if !line.is_empty() {
            result.push(line);
        }
    }
    result
}

fn scalar_display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

fn render_compact_summary(
    rows: &[(&str, String, Option<&str>)],
    tip: Option<&str>,
    max_width: usize,
) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (label, status, reason) in rows {
        let label = sanitize(label, MAX_DIAGNOSTIC_CHARS).replace(['\n', '\r'], " ");
        let label = if label.trim().is_empty() {
            "-"
        } else {
            label.trim()
        };
        for line in wrap_text(&format!("{label}: {status}"), max_width) {
            out.push_str(&line);
            out.push('\n');
        }
        if let Some(reason) = reason {
            for line in wrap_text(&sanitize(reason, MAX_DIAGNOSTIC_CHARS), max_width) {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    if let Some(tip) = tip.filter(|tip| !tip.is_empty()) {
        let cleaned = sanitize(tip, MAX_DIAGNOSTIC_CHARS);
        for line in wrap_text(&cleaned, max_width) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn truncate_display(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let room = width.saturating_sub(1);
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > room {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// A box-drawn table sized by terminal display width, shared by CLI and TUI.
pub(crate) fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.width()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.width());
        }
    }
    let mut out = String::new();
    out.push('┌');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┬' } else { '┐' });
    }
    out.push('\n');
    out.push('│');
    for (h, w) in headers.iter().zip(widths.iter()) {
        let pad = w.saturating_sub(h.width());
        out.push_str(&format!(" {h}{} │", " ".repeat(pad)));
    }
    out.push('\n');
    out.push('├');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┼' } else { '┤' });
    }
    out.push('\n');
    for row in rows {
        out.push('│');
        for (cell, w) in row.iter().zip(widths.iter()) {
            let pad = w.saturating_sub(cell.width());
            out.push_str(&format!(" {cell}{} │", " ".repeat(pad)));
        }
        out.push('\n');
    }
    out.push('└');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┴' } else { '┘' });
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ready_summary::ReadySummary;

    fn max_line_width(text: &str) -> usize {
        text.lines().map(|line| line.width()).max().unwrap_or(0)
    }

    fn check_box_lines_are_aligned(text: &str) {
        let lines: Vec<_> = text.lines().collect();
        let start = lines.iter().position(|line| line.starts_with('┌')).unwrap();
        let end = lines[start..]
            .iter()
            .position(|line| line.starts_with('└'))
            .map(|offset| start + offset)
            .unwrap();
        let box_lines = &lines[start..=end];
        assert!(!box_lines.is_empty());
        assert!(box_lines
            .iter()
            .all(|line| { line.chars().next().is_some_and(|c| "┌│├└".contains(c)) }));
        let width = box_lines[0].width();
        assert!(box_lines.iter().all(|line| line.width() == width));
    }

    #[test]
    fn multiline_long_failure_is_wrapped_under_its_row_and_bounded() {
        let mut summary = ReadySummary::new("builtin");
        summary.image_source = Some(StepStatus::Failed(format!(
            "source unavailable\n{}",
            "/very/long/path/".repeat(40)
        )));
        let rendered = render_ready_summary_with_width(&summary, 40);
        check_box_lines_are_aligned(&rendered);
        assert!(max_line_width(&rendered) <= 40);
        assert_eq!(rendered.matches("source unavailable").count(), 1);
        assert_eq!(rendered.matches("Tip:").count(), 1);
        assert!(rendered.contains("failed"));
    }

    #[test]
    fn cjk_and_emoji_use_terminal_display_width_for_padding() {
        let rows = [("界🙂", &StepStatus::Done)];
        let rendered = render_summary_box_with_width("界 🙂", &rows, 40);
        check_box_lines_are_aligned(&rendered);
        assert!(rendered.lines().all(|line| line.width() == 40));
    }

    #[test]
    fn narrow_viewports_at_twenty_and_forty_columns_stay_bounded() {
        for width in [20, 40] {
            let rows = [(
                "A very long stage label",
                &StepStatus::Failed("x".repeat(100)),
            )];
            let rendered = render_summary_box_with_width("Ready Summary", &rows, width);
            if width >= MIN_BOX_WIDTH {
                check_box_lines_are_aligned(&rendered);
            }
            assert!(max_line_width(&rendered) <= width);
        }
    }

    #[test]
    fn malicious_controls_and_ansi_are_neutralized() {
        let rows = [(
            "Stage\u{1b}[2J",
            &StepStatus::Failed("bad\u{1b}[31mred\u{1b}[0m\u{7}\r\nnext".into()),
        )];
        let rendered = render_summary_box_with_width("Ready", &rows, 40);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(rendered.contains("badred"));
        assert!(rendered.contains("next"));
        check_box_lines_are_aligned(&rendered);
        assert!(max_line_width(&rendered) <= 40);
    }

    #[test]
    fn failed_stage_reason_and_tip_each_appear_once() {
        let mut summary = ReadySummary::new("builtin");
        summary.image_source = Some(StepStatus::Failed("daemon unavailable".into()));
        let rendered = render_ready_summary_with_width(&summary, 80);
        assert_eq!(rendered.matches("daemon unavailable").count(), 1);
        assert_eq!(rendered.matches("Image source").count(), 1);
        assert_eq!(rendered.matches("Tip:").count(), 1);
    }

    #[test]
    fn tiny_viewports_never_emit_wider_lines_and_stack_the_status() {
        let rows = [(
            "Image source",
            &StepStatus::Failed("daemon unavailable".into()),
        )];
        for width in [0, 1, 10, 14, 15, 20, 29] {
            let rendered = render_summary_box_with_width("Ready", &rows, width);
            assert!(max_line_width(&rendered) <= width, "{width}: {rendered:?}");
        }
        let rendered = render_summary_box_with_width("Ready", &rows, 20);
        assert!(rendered.contains("Image source: failed"));
        assert!(rendered.contains("daemon unavailable"));
    }

    #[test]
    fn wide_characters_do_not_overflow_narrow_cells() {
        let rows = [("界界", &StepStatus::Done)];
        for width in [15, 20, 29, 30] {
            let rendered = render_summary_box_with_width("Ready", &rows, width);
            if width >= MIN_BOX_WIDTH {
                check_box_lines_are_aligned(&rendered);
            }
            assert!(max_line_width(&rendered) <= width, "{width}: {rendered:?}");
        }
    }

    #[test]
    fn crlf_is_one_separator_and_blank_paragraphs_are_collapsed() {
        let rows = [("stage", &StepStatus::Failed("a\r\n\r\n\r\nb\n".into()))];
        let rendered = render_summary_box_with_width("Ready", &rows, 40);
        assert!(rendered.lines().any(|line| line.contains("a ")));
        assert!(rendered.contains("b"));
        assert_eq!(
            rendered.lines().filter(|line| line.contains("│  ")).count(),
            2
        );
    }

    #[test]
    fn trailing_and_repeated_tip_lines_are_normalized() {
        let rows = [("stage", "done".to_string(), None)];
        let rendered = render_summary_values("Ready", &rows, 40, Some("first\n\n\nsecond\n"));
        assert!(rendered.contains("Tip: first\n     second\n"));
        assert!(!rendered.ends_with("     \n"));
    }

    #[test]
    fn whitespace_labels_keep_a_status_row() {
        let rows = [("\t  ", &StepStatus::Done)];
        let rendered = render_summary_box_with_width("Ready", &rows, 40);
        assert!(rendered.lines().any(|line| line.contains("done")));
    }

    #[test]
    fn malformed_escape_sequences_do_not_eat_error_text() {
        let rows = [("stage", &StepStatus::Failed(
            "real error \u{1b}]0;title with rest\nnext \u{1b}[ 12 failed: disk full \u{1b}(B ok \u{1b}P dcs payload \u{1b}\\ end".into(),
        ))];
        let rendered = render_summary_box_with_width("Ready", &rows, 60);
        for text in ["real error", "next", "failed: disk full", "ok", "end"] {
            assert!(rendered.contains(text), "missing {text:?}: {rendered:?}");
        }
        assert!(!rendered.contains("dcs payload"));
        assert!(!rendered.contains("B ok"));
    }

    #[test]
    fn bidi_and_unicode_line_separators_are_neutralized() {
        let rows = [(
            "stage",
            &StepStatus::Failed("\u{202e}evil\u{200b}zw\u{2028}ls\u{2029}ps".into()),
        )];
        let rendered = render_summary_box_with_width("Ready", &rows, 40);
        for ch in ['\u{202e}', '\u{200b}', '\u{2028}', '\u{2029}'] {
            assert!(!rendered.contains(ch));
        }
        assert!(rendered.contains("evil"));
    }

    #[test]
    fn tips_and_reasons_trim_trailing_blanks_and_collapse_repeated_blanks() {
        let rows = [("stage", &StepStatus::Failed("line\n\n\nnext\n".into()))];
        let rendered = render_summary_box_with_width("Ready", &rows, 40);
        assert_eq!(rendered.lines().filter(|line| line == &"│    │").count(), 0);
        assert!(!rendered.ends_with("     \n"));
    }

    #[test]
    fn tui_ready_width_reserves_sidebar_and_execution_window_borders() {
        assert_eq!(tui_ready_render_width(120), 88);
        assert_eq!(tui_ready_render_width(40), 28);
        assert_eq!(tui_ready_render_width(1), 0);
    }

    #[test]
    fn ready_event_text_sanitization_removes_controls_and_bounds_length() {
        let text = sanitize_ready_text("bad\u{1b}[2J\u{202e}text\r\nnext");
        assert_eq!(text, "bad text\nnext");
        let c1 = sanitize_ready_text("a\u{009b}2Jb\u{009d}0;title\u{7}c");
        assert_eq!(c1, "abc");
        assert!(sanitize_ready_text(&"x".repeat(MAX_DIAGNOSTIC_CHARS + 20)).ends_with('…'));
    }

    #[test]
    fn emoji_sequence_wrapping_uses_the_same_scalar_width_measure() {
        let sequence = "👨‍👩‍👧‍👦";
        let lines = wrap_text(sequence, 4);
        assert!(lines.iter().all(|line| scalar_display_width(line) <= 4));
        assert_eq!(lines.concat(), sequence);
    }
}
