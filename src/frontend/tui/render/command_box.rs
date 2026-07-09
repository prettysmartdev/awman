//! Command box, suggestion row, and shared text-truncation/color helpers
//! used by other render widgets (status-log entries, path display).

use super::*;

/// Render the command box.
///
/// Matches old amux:
/// - 3-row rounded border
/// - Title `" command "` when focused, `" command (inactive) "` when blurred
/// - Border + prefix Cyan when focused; DarkGray when blurred
/// - When the active tab's command is Running and the command box still has
///   focus: show a DarkGray hint to open a new tab instead of the input
/// - When `input_error` is set: replace the input body with `"  {err}"` in Red
///   and suppress the cursor
/// - Newlines in the input render as `↵` (U+21B5) so multi-line input doesn't
///   break the single visible row
/// - Cursor sits at `area.x + 1 (border) + 2 ("> " prefix) + cursor_col` and
///   is suppressed if it would overlap the right border
pub(super) fn render_command_box(app: &App, area: Rect, frame: &mut Frame) {
    let is_running = matches!(
        app.active_tab().execution_phase,
        tabs::ExecutionPhase::Running { .. }
    );
    let focused = app.focus == Focus::CommandBox && !is_running;

    let border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = if focused {
        " command "
    } else {
        " command (inactive) "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Locked-during-running hint takes precedence over input rendering.
    if is_running && app.focus == Focus::CommandBox {
        let line = Line::from(Span::styled(
            "  Press Ctrl+T to run another command in a new tab",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(line), inner);
        return;
    }

    if let Some(ref err) = app.input_error {
        let line = Line::from(Span::styled(
            format!("  {err}"),
            Style::default().fg(Color::Red),
        ));
        frame.render_widget(Paragraph::new(line), inner);
        return;
    }

    // E.2: ghost text when empty, focused, and idle/done.
    if app.command_input.text.is_empty() && focused {
        let show_ghost = matches!(
            app.active_tab().execution_phase,
            ExecutionPhase::Idle | ExecutionPhase::Done { .. }
        );
        if show_ghost {
            let prefix = Span::styled("> ", Style::default().fg(Color::Cyan));
            let ghost = Span::styled("q to quit", Style::default().fg(Color::DarkGray));
            let line = Line::from(vec![prefix, ghost]);
            frame.render_widget(Paragraph::new(line), inner);
            let cursor_x = area.x + 1 + 2;
            let cursor_y = area.y + 1;
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            return;
        }
    }

    let prefix = Span::styled("> ", Style::default().fg(Color::Cyan));
    let display_text = app.command_input.text.replace('\n', "\u{21b5}");

    // E.1: horizontal scroll for long input.
    let visible_width = inner.width.saturating_sub(2) as usize; // subtract prefix "> "
    let cursor_col = {
        let text_before_cursor = &app.command_input.text[..app.command_input.cursor];
        unicode_width::UnicodeWidthStr::width(text_before_cursor.replace('\n', "\u{21b5}").as_str())
    };
    let scroll_offset = command_box_scroll_offset(cursor_col, visible_width);
    let visible_text: String = display_text.chars().skip(scroll_offset).collect();
    let line = Line::from(vec![prefix, Span::raw(visible_text)]);
    frame.render_widget(Paragraph::new(line), inner);

    if focused && app.active_dialog.is_none() {
        let display_cursor_x = cursor_col.saturating_sub(scroll_offset) as u16;
        let cursor_x = area.x + 1 + 2 + display_cursor_x;
        let cursor_y = area.y + 1;
        if cursor_x < area.x + area.width.saturating_sub(1) {
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
    }
}

/// How many leading characters of the command-box input to hide so the
/// cursor stays visible (E.1 horizontal scroll). With a zero-width box
/// (degenerate terminal) everything scrolls off and the cursor pins to
/// column 0 — must never underflow.
pub(super) fn command_box_scroll_offset(cursor_col: usize, visible_width: usize) -> usize {
    (cursor_col + 1).saturating_sub(visible_width)
}

/// Render the 1-row suggestion / context line below the command box.
///
/// Dual purpose:
/// - When the command box is focused AND there are autocomplete suggestions:
///   render them separated by `"  ·  "` in DarkGray with each suggestion in
///   Cyan.
/// - Otherwise: fall back to a `"  CWD: {path}"` line (or `"  Using
///   Worktree: {path}"` when the active tab is bound to a worktree).
pub(super) fn render_suggestion_row(app: &App, area: Rect, frame: &mut Frame) {
    let show_suggestions = app.focus == Focus::CommandBox && !app.suggestion_row.is_empty();

    if show_suggestions {
        let mut spans: Vec<Span> = Vec::with_capacity(app.suggestion_row.len() * 2);
        let catalogue = crate::command::dispatch::catalogue::CommandCatalogue::get();
        let command_path: Vec<&str> = app
            .command_input
            .text
            .split_whitespace()
            .take_while(|t| !t.starts_with('-'))
            .collect();
        let cmd_spec = catalogue.lookup(&command_path);
        for (i, s) in app.suggestion_row.iter().enumerate() {
            let sep = if i == 0 {
                Span::raw("  ")
            } else {
                Span::styled("  \u{00b7}  ", Style::default().fg(Color::DarkGray))
            };
            spans.push(sep);
            spans.push(Span::styled(s.as_str(), Style::default().fg(Color::Cyan)));
            // F.2: append flag hint with em-dash if available.
            let flag_name = s.strip_prefix("--").unwrap_or(s);
            if let Some(spec) = cmd_spec.and_then(|cs| cs.find_flag(flag_name)) {
                if !spec.help.is_empty() {
                    spans.push(Span::styled(
                        format!(" \u{2014} {}", spec.help),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
        }
        let para = Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
        return;
    }

    // Context fallback: show worktree path (if active) or working directory.
    //
    // Three sources, in priority order:
    //   1. The shared active-worktree path published by the worktree-lifecycle
    //      frontend while a workflow runs in a worktree.
    //   2. The tab session's working_dir when it differs from git_root (the
    //      session was opened directly on a worktree path — e.g. exec workflow
    //      with --worktree opened a fresh session there).
    //   3. The CWD itself.
    let tab = app.active_tab();
    let working_dir = tab.session.working_dir();
    let git_root = tab.session.git_root();
    let active_worktree: Option<std::path::PathBuf> =
        tab.active_worktree_path.lock().ok().and_then(|g| g.clone());

    let para = if let Some(wt) = active_worktree {
        let label = "  Using worktree: ";
        let max_path_w = (area.width as usize).saturating_sub(label.len() + 2);
        let wt_str = truncate_middle(&wt.to_string_lossy(), max_path_w);
        Paragraph::new(Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Blue)),
            Span::styled(wt_str, Style::default().fg(Color::DarkGray)),
        ]))
    } else if working_dir != git_root {
        let label = "  Using worktree: ";
        let max_path_w = (area.width as usize).saturating_sub(label.len() + 2);
        let wt_str = truncate_middle(&working_dir.to_string_lossy(), max_path_w);
        Paragraph::new(Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Blue)),
            Span::styled(wt_str, Style::default().fg(Color::DarkGray)),
        ]))
    } else {
        let label = "  CWD: ";
        let max_path_w = (area.width as usize).saturating_sub(label.len() + 2);
        let cwd_str = truncate_middle(&working_dir.to_string_lossy(), max_path_w);
        Paragraph::new(Line::from(vec![
            Span::styled(label, Style::default().fg(Color::DarkGray)),
            Span::styled(cwd_str, Style::default().fg(Color::DarkGray)),
        ]))
    };
    frame.render_widget(para, area);
}

/// Truncate a string to at most `max` characters, replacing the middle with an
/// ellipsis (`…`) when the string exceeds the limit.
pub(super) fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let ellipsis = "\u{2026}";
    let available = max.saturating_sub(1); // 1 for the ellipsis char
    let prefix_len = available / 2;
    let suffix_len = available - prefix_len;
    let prefix: String = s.chars().take(prefix_len).collect();
    let suffix: String = s
        .chars()
        .rev()
        .take(suffix_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}{ellipsis}{suffix}")
}

/// Map message level to display color.
pub(super) fn status_level_color(level: &crate::data::message::MessageLevel) -> Color {
    use crate::data::message::MessageLevel;
    match level {
        MessageLevel::Info => Color::White,
        MessageLevel::Warning => Color::Yellow,
        MessageLevel::Error => Color::Red,
        MessageLevel::Success => Color::Green,
    }
}
