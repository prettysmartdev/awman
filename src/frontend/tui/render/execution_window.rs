//! Execution window rendering: the main output pane (status log or agents
//! dashboard), text selection highlighting, and buffer-grid capture for
//! copy support.

use super::*;

/// Render the execution window — rounded border with the phase label as the
/// left-aligned title; border color from `window_border_color(phase, focused)`.
///
/// Body content:
/// - Idle (and the status log is empty): a 3-line welcome stub in DarkGray.
/// - Otherwise: the status log entries, colored per level, with `Wrap{trim:false}`.
///
/// While the container overlay is not Maximized the window's text is mouse-
/// selectable: an active selection is highlighted with `Modifier::REVERSED`,
/// the copy hint is shown on the bottom border, and the inner rect plus the
/// visible text grid are published on the tab so `handle_mouse_event` can
/// start/extend selections (mirrors `render_container_maximized`).
pub(super) fn render_execution_window(app: &mut App, area: Rect, frame: &mut Frame) {
    let tab = app.active_tab();
    let focused = app.focus == Focus::ExecutionWindow;
    // The execution window borrows the ACP identity color in its one green
    // (focused + Done) state when the focused agent slot is an ACP window.
    let acp = tab.focused_slot().is_some_and(|s| s.is_acp());
    let border_color = window_border_color(&tab.execution_phase, focused, acp);
    let title = phase_label(&tab.execution_phase);

    let container_maximized = tab.container_overlay_active();
    // A selection only belongs to this window while the container overlay
    // isn't covering it; when the overlay is active the selection is the
    // overlay's.
    let selection = if container_maximized {
        None
    } else {
        tab.mouse_selection.clone()
    };

    let mut block = Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    if selection.is_some() {
        block = block.title_bottom(
            Line::from(Span::styled(
                " CTRL-Y to copy/yank text ",
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The agents dashboard takes priority once `agents status` has drawn it.
    let has_dashboard = tab
        .shared
        .agents_dashboard
        .lock()
        .map(|d| d.is_some())
        .unwrap_or(false);

    let log_empty = tab
        .shared
        .status_log
        .lock()
        .map(|log| log.is_empty())
        .unwrap_or(true);

    if has_dashboard {
        render_agents_dashboard(tab, inner, frame);
    } else if matches!(tab.execution_phase, ExecutionPhase::Idle) && log_empty {
        // Three-line welcome stub matching old awman exactly.
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Welcome to awman.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  Running `awman ready` to check your environment...",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        render_output_content(tab, inner, frame);
    }

    if let Some(ref sel) = selection {
        apply_selection_highlight(frame.buffer_mut(), inner, sel);
    }

    // Publish the inner rect and visible text grid for the mouse handler.
    // When Maximized the overlay covers this window, so clear the rect to
    // keep clicks from starting an execution-window selection underneath.
    let grid = if container_maximized {
        Vec::new()
    } else {
        capture_buffer_grid(frame.buffer_mut(), inner)
    };
    let tab = app.active_tab_mut();
    tab.exec_inner_area = if container_maximized {
        None
    } else {
        Some(inner)
    };
    tab.exec_window_grid = grid;
}

/// Overlay `Modifier::REVERSED` on the cells of an active selection.
/// Selection coordinates are relative to `area` (the window's inner rect).
pub(super) fn apply_selection_highlight(buf: &mut Buffer, area: Rect, sel: &tabs::TextSelection) {
    let norm = Some(container_view::normalize_selection(sel));
    for row in 0..area.height {
        for col in 0..area.width {
            if container_view::cell_in_selection(norm, row, col) {
                if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                    cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

/// Snapshot the rendered cell contents of `area` from the frame buffer into
/// a grid of per-cell strings (empty cells become `" "`), the same shape
/// `capture_vt100_snapshot` produces for the container overlay. Reading back
/// the buffer guarantees the copied text matches what was displayed,
/// including ratatui's word wrapping.
pub(super) fn capture_buffer_grid(buf: &Buffer, area: Rect) -> Vec<Vec<String>> {
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|col| {
                    buf.cell((area.x + col, area.y + row))
                        .map(|c| {
                            let s = c.symbol();
                            if s.is_empty() {
                                " ".to_string()
                            } else {
                                s.to_string()
                            }
                        })
                        .unwrap_or_else(|| " ".to_string())
                })
                .collect()
        })
        .collect()
}

/// Render the status-log lines into the execution window.
///
/// PTY/container output is rendered exclusively through the container overlay
/// widget (`render_container_maximized` / `render_container_minimized`), never
/// here — that prevents Claude's TUI from bleeding into the execution window.
///
/// Long lines are wrapped (preserving leading whitespace). The visual scroll
/// offset is computed against wrapped row count so `scroll_offset` is in
/// "screen rows", not log entries — matches old amux's behavior where the
/// scroll is anchored to the bottom and increasing offset moves toward older.
fn render_output_content(tab: &tabs::Tab, area: Rect, frame: &mut Frame) {
    let log = match tab.shared.status_log.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if log.is_empty() {
        return;
    }

    if tab.status_log_collapsed {
        let last = &log[log.len() - 1];
        let color = command_box::status_level_color(&last.level);
        let line = Line::from(Span::styled(&last.text, Style::default().fg(color)));
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }

    let lines: Vec<Line> = log
        .iter()
        .map(|entry| {
            let color = command_box::status_level_color(&entry.level);
            Line::from(Span::styled(
                entry.text.as_str(),
                Style::default().fg(color),
            ))
        })
        .collect();

    let inner_height = area.height as usize;
    let inner_width = area.width as usize;
    let total_visual: usize = if inner_width == 0 {
        lines.len()
    } else {
        lines
            .iter()
            .map(|l| {
                let w = l.width();
                if w == 0 {
                    1
                } else {
                    w.div_ceil(inner_width)
                }
            })
            .sum()
    };
    let max_scroll = total_visual.saturating_sub(inner_height);
    let effective_offset = tab.scroll_offset.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(effective_offset);

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y as u16, 0));
    frame.render_widget(para, area);
}

/// Render the `agents status` dashboard: the runtime header, a `Table` of
/// agents (or the empty / unavailable state), a footer, and the run's tip.
///
/// The columns are the CLI's `STATUS_COLUMNS` with TAB inserted after
/// SOURCE, filled from the same Layer 2 cell builders, so the two frontends
/// agree on content. NAME and IMAGE take the space that is left; every
/// other column is sized to its content. Table cells truncate rather than
/// wrap, so a narrow tab never breaks a row across lines. When the rows do
/// not fit they scroll with the execution window's scroll keys, anchored to
/// the bottom like the status log.
fn render_agents_dashboard(tab: &tabs::Tab, area: Rect, frame: &mut Frame) {
    let dash = match tab.shared.agents_dashboard.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let Some(data) = dash.as_ref() else {
        return;
    };
    let overview = &data.overview;

    let header_color = if overview.available {
        Color::Cyan
    } else {
        Color::Red
    };
    let dim = Style::default().fg(Color::DarkGray);

    // Header + blank, body, blank + footer + tip.
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", overview.header_line()),
            Style::default()
                .fg(header_color)
                .add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );

    // When the runtime is unavailable the header already names it and the
    // reason, and the body stays empty; a watch keeps retrying, so a daemon
    // restart shows up on the next tick.
    let mut scroll_note = None;
    if overview.available && overview.agents.is_empty() {
        let lines: Vec<Line> = overview
            .empty_state_lines()
            .into_iter()
            .map(|l| Line::from(Span::styled(format!(" {l}"), dim)))
            .collect();
        frame.render_widget(Paragraph::new(lines), chunks[1]);
    } else if overview.available {
        scroll_note = render_agents_table(overview, tab.scroll_offset, chunks[1], frame);
    }

    let mut footer = if data.watching {
        format!(
            " Refreshing every {}s · Esc to stop",
            crate::command::commands::agents::WATCH_INTERVAL.as_secs()
        )
    } else {
        format!(
            " Collected at {}",
            overview
                .collected_at
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
        )
    };
    if let Some(note) = scroll_note {
        footer.push_str(" · ");
        footer.push_str(&note);
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(footer, dim)),
            Line::from(Span::styled(format!(" Tip: {}", overview.tip), dim)),
        ]),
        chunks[2],
    );
}

/// Draw the agents table into `area`, skipping rows per `scroll_offset`
/// (rows from the bottom, as in the status log). Returns a "rows a–b of n"
/// note when not every row fits.
fn render_agents_table(
    overview: &crate::command::commands::agents::AgentsOverview,
    scroll_offset: usize,
    area: Rect,
    frame: &mut Frame,
) -> Option<String> {
    use crate::command::commands::agents::{AgentState, STATUS_COLUMNS};

    // STATUS_COLUMNS is [glyph, AGENT ID, NAME, SOURCE, IMAGE, UPTIME, CPU, MEM];
    // TAB goes after SOURCE.
    const TAB_AT: usize = 4;
    const NAME_AT: usize = 2;
    const IMAGE_AT: usize = 5;

    let mut headers: Vec<&str> = STATUS_COLUMNS.to_vec();
    headers.insert(TAB_AT, "TAB");
    let rows: Vec<(AgentState, Vec<String>)> = overview
        .agents
        .iter()
        .map(|agent| {
            let mut cells = agent.status_cells();
            let tab = agent
                .tab_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into());
            cells.insert(TAB_AT, tab);
            (agent.state, cells)
        })
        .collect();

    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for (_, cells) in &rows {
        for (w, cell) in widths.iter_mut().zip(cells) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let constraints: Vec<Constraint> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| match i {
            NAME_AT => Constraint::Min((*w).min(16) as u16),
            IMAGE_AT => Constraint::Fill(1),
            _ => Constraint::Length(*w as u16),
        })
        .collect();

    // One line for the header row.
    let visible = (area.height as usize).saturating_sub(1);
    let max_skip = rows.len().saturating_sub(visible);
    let skip = max_skip.saturating_sub(scroll_offset.min(max_skip));
    let shown = rows.len().min(visible);

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(
        headers
            .iter()
            .map(|h| Cell::from(*h).style(header_style))
            .collect::<Vec<_>>(),
    );
    let table_rows: Vec<Row> = rows
        .iter()
        .skip(skip)
        .take(visible)
        .map(|(state, cells)| {
            let glyph_color = match state {
                AgentState::Running => Color::Green,
                AgentState::Stuck => Color::Yellow,
                AgentState::Stopped => Color::DarkGray,
            };
            Row::new(cells.iter().enumerate().map(|(i, cell)| {
                let c = Cell::from(cell.as_str());
                if i == 0 {
                    c.style(
                        Style::default()
                            .fg(glyph_color)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    c
                }
            }))
        })
        .collect();

    let table = Table::new(table_rows, constraints)
        .header(header)
        .column_spacing(2);
    // A one-column gutter so the glyph does not touch the window border.
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(1),
        ..area
    };
    frame.render_widget(table, inner);

    (rows.len() > shown).then(|| {
        format!(
            "rows {}–{} of {} (↑/↓ to scroll)",
            skip + 1,
            skip + shown,
            rows.len()
        )
    })
}
