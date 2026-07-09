//! Git sidebar widget rendering: the changed-files list and per-line diff
//! stat formatting.

use super::*;

/// Render the 1-row status hint bar above the command box.
///
/// Content is a `(phase, focus, container)` decision matrix copied from
/// old amux: tells the user which keybinding is most relevant right now
/// (Esc to deselect, ↑ to focus the window, ctrl-m to cycle the container,
/// etc.). Background is forced black so the row stands out against the
/// surrounding chrome.
/// Render the git sidebar into the right chunk: a rounded, green-bordered
/// block titled with a condensed `git status` line (branch + change count),
/// containing bold `+A -D` totals and a color-coded per-file change list.
pub(super) fn render_git_sidebar(frame: &mut Frame, area: Rect, summary: &Option<GitDiffSummary>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green))
        .title(Span::styled(
            format!(" {} ", git_sidebar::sidebar_title(summary)),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(summary) = summary else {
        let line = Line::from(Span::styled(
            "no git data",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
        frame.render_widget(Paragraph::new(line), inner);
        return;
    };

    // Bold `+A -D` totals on the first inner row.
    let title = Line::from(vec![
        Span::styled(
            format!("+{}", summary.total_additions),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("-{}", summary.total_deletions),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    ]);

    // Split the inner area into the title row and the file list. The list is
    // clipped to the visible rows (extra files are dropped for now).
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    frame.render_widget(Paragraph::new(title), rows[0]);

    let inner_width = inner.width as usize;
    let list_height = rows[1].height as usize;
    let items: Vec<ListItem> = summary
        .files
        .iter()
        .take(list_height)
        .map(|f| ListItem::new(git_file_line(f, inner_width)))
        .collect();
    frame.render_widget(List::new(items), rows[1]);
}

/// Build a single sidebar file line: a fixed `+A -D ` stat prefix followed by
/// the (possibly truncated) path, all in the change type's accent color.
pub(super) fn git_file_line(entry: &GitFileEntry, inner_width: usize) -> Line<'static> {
    let color = match entry.change_type {
        GitFileChangeType::Added => Color::Green,
        GitFileChangeType::Deleted => Color::Red,
        GitFileChangeType::Modified => Color::Blue,
    };
    let stat = format!("+{} -{} ", entry.additions, entry.deletions);
    let suffix = if entry.binary { " (binary)" } else { "" };
    let reserved = stat.chars().count() + suffix.chars().count();
    let avail = inner_width.saturating_sub(reserved);
    let path = truncate_path(&entry.path, avail);
    Line::from(vec![
        Span::styled(stat, Style::default().fg(color)),
        Span::styled(format!("{path}{suffix}"), Style::default().fg(color)),
    ])
}

/// Truncate `path` to at most `max` display columns, appending `…` when cut.
pub(super) fn truncate_path(path: &str, max: usize) -> String {
    let len = path.chars().count();
    if len <= max {
        return path.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let truncated: String = path.chars().take(max - 1).collect();
    format!("{truncated}\u{2026}")
}
