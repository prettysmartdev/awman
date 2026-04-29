use crate::tui::state::{
    App, TabState, ContainerWindowState, Dialog, ExecutionPhase, Focus, LastContainerSummary,
};
use crate::workflow::{StepStatus, WorkflowState};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
    Frame,
};

/// Top-level render function: draws the full TUI for one frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Vertical split: tab bar (3 rows) + main content area.
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);
    let tab_bar_area = vert[0];
    let main_area = vert[1];

    draw_tab_bar(frame, app, tab_bar_area);

    let tab = app.active_tab_mut();

    // Determine if we need a minimized container bar or a summary bar.
    let show_minimized_bar = tab.container_window == ContainerWindowState::Minimized;
    let show_summary = !show_minimized_bar
        && tab.container_window == ContainerWindowState::Hidden
        && tab.last_container_summary.is_some();
    let extra_bar_height = if show_minimized_bar || show_summary { 3 } else { 0 };

    // Determine workflow strip height (0 if no workflow active).
    let workflow_strip_height = if tab.workflow.is_some() {
        workflow_strip_height(tab.workflow.as_ref().unwrap())
    } else {
        0
    };

    let mut constraint_list: Vec<Constraint> = vec![Constraint::Min(5)];
    if extra_bar_height > 0 {
        constraint_list.push(Constraint::Length(extra_bar_height));
    }
    if workflow_strip_height > 0 {
        constraint_list.push(Constraint::Length(workflow_strip_height));
    }
    constraint_list.push(Constraint::Length(1)); // status bar
    constraint_list.push(Constraint::Length(3)); // command box
    constraint_list.push(Constraint::Length(1)); // suggestions

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraint_list)
        .split(main_area);

    let mut idx = 0usize;
    let exec_area = chunks[idx];
    idx += 1;

    draw_exec_window(frame, tab, exec_area);

    if extra_bar_height > 0 {
        if show_minimized_bar {
            draw_minimized_container_bar(frame, tab, chunks[idx]);
        } else if show_summary {
            draw_container_summary(frame, tab.last_container_summary.as_ref().unwrap(), chunks[idx]);
        }
        idx += 1;
    }

    if workflow_strip_height > 0 {
        if let Some(wf) = tab.workflow.as_ref() {
            draw_workflow_strip(frame, wf, tab.workflow_current_step.as_deref(), chunks[idx]);
        }
        idx += 1;
    }

    draw_status_bar(frame, tab, chunks[idx]);
    idx += 1;
    draw_command_box(frame, tab, chunks[idx]);
    idx += 1;
    draw_suggestions(frame, tab, chunks[idx]);

    if tab.container_window == ContainerWindowState::Maximized {
        draw_container_window(frame, tab, exec_area);
    }

    if tab.dialog != Dialog::None {
        draw_dialog(frame, tab, area);
    }
}

/// Calculate the inner dimensions of the container window for a given terminal size.
///
/// This mirrors the layout used in `draw_container_window` so the vt100 parser
/// and PTY are sized to match the actual rendered area.
///
/// `extra_rows` accounts for additional UI strips that reduce the exec area height
/// (e.g. the workflow status strip). Pass 0 when no extra strips are visible.
pub fn calculate_container_inner_size(term_cols: u16, term_rows: u16, extra_rows: u16) -> (u16, u16) {
    // No sidebar. Tab bar takes 3 rows at top.
    // Fixed rows below tab bar: status(1) + cmd(3) + suggest(1) = 5.
    let exec_height = term_rows.saturating_sub(3 + 5 + extra_rows);
    // Container window: 95% of exec area, centered.
    let container_height = (exec_height * 95 / 100).max(5);
    let container_width = (term_cols * 95 / 100).max(10);
    // Inner area excludes borders.
    let inner_rows = container_height.saturating_sub(2);
    let inner_cols = container_width.saturating_sub(2);
    (inner_cols, inner_rows)
}

// --- Tab bar (horizontal, top) ---

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    const TAB_WIDTH: u16 = 20;
    for (i, tab) in app.tabs.iter().enumerate() {
        let x = area.x + (i as u16) * TAB_WIDTH;
        if x + TAB_WIDTH > area.x + area.width {
            break;
        }
        let is_active = i == app.active_tab_idx;
        // All tabs share the same 3-row height, flush to the top of the tab bar area.
        let tab_area = Rect { x, y: area.y, width: TAB_WIDTH, height: 3 };
        let color = tab.tab_color(is_active);
        let project = tab.tab_project_name();
        let subcmd = tab.tab_subcommand_label(TAB_WIDTH, is_active);

        let (border_style, title_style, content_style) = if is_active {
            (
                Style::default().fg(color),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                Style::default().fg(color),
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
            )
        };

        let title_text = if is_active {
            format!(" ➡ {} ", project)
        } else {
            format!(" {} ", project)
        };

        let borders = if is_active {
            Borders::TOP | Borders::LEFT | Borders::RIGHT
        } else {
            Borders::ALL
        };

        let block = Block::default()
            .title(Span::styled(title_text, title_style))
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(border_style);

        let content = Paragraph::new(Line::from(Span::styled(
            format!(" {} ", subcmd),
            content_style,
        )))
        .block(block);
        frame.render_widget(content, tab_area);
    }
}

// --- Execution window (outer window) ---

fn draw_exec_window(frame: &mut Frame, tab: &TabState, area: Rect) {
    let border_color = tab.window_border_color();
    let border_style = Style::default().fg(border_color);

    // Calculate how many visual rows fit in the window (subtract borders).
    let inner_height = area.height.saturating_sub(2) as usize;

    let phase_label = match &tab.phase {
        ExecutionPhase::Idle => " amux ".to_string(),
        ExecutionPhase::Running { command } => format!(" ● running: {} ", command),
        ExecutionPhase::Done { command } => format!(" ✓ done: {} ", command),
        ExecutionPhase::Error { command, exit_code } => {
            format!(" ✗ error: {} (exit {}) ", command, exit_code)
        }
    };

    let block = Block::default()
        .title(phase_label)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);

    let inner_width = area.width.saturating_sub(2) as usize; // exclude borders

    let lines: Vec<Line> = if tab.output_lines.is_empty() {
        if matches!(tab.phase, ExecutionPhase::Idle) {
            vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Welcome to amux.",
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(vec![Span::styled(
                    "  Running `amux ready` to check your environment...",
                    Style::default().fg(Color::DarkGray),
                )]),
            ]
        } else {
            vec![]
        }
    } else {
        tab.output_lines
            .iter()
            .map(|l| Line::from(l.as_str()))
            .collect()
    };

    // Calculate how many visual rows the content takes, using display width
    // (via Line::width()) instead of byte length.
    let total_visual: usize = if inner_width == 0 {
        lines.len()
    } else {
        lines
            .iter()
            .map(|l| {
                let w = l.width();
                if w == 0 { 1 } else { (w + inner_width - 1) / inner_width }
            })
            .sum()
    };
    let max_scroll = total_visual.saturating_sub(inner_height);
    let effective_offset = tab.scroll_offset.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(effective_offset);

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y as u16, 0));
    frame.render_widget(para, area);
}

// --- Container window (overlay on top of outer window) ---

fn draw_container_window(frame: &mut Frame, tab: &mut TabState, outer_area: Rect) {
    // Container window takes 95% of the outer window's width and height, centered.
    let container_height = (outer_area.height * 95 / 100).max(5);
    let container_width = (outer_area.width * 95 / 100).max(10);
    let offset_x = (outer_area.width.saturating_sub(container_width)) / 2;
    let offset_y = (outer_area.height.saturating_sub(container_height)) / 2;
    let container_area = Rect {
        x: outer_area.x + offset_x,
        y: outer_area.y + offset_y,
        width: container_width,
        height: container_height,
    };

    // Clear the area under the container window.
    frame.render_widget(Clear, container_area);

    // Build title strings.
    let agent_name = tab
        .container_info
        .as_ref()
        .map(|i| i.agent_display_name.as_str())
        .unwrap_or("Agent");
    let left_title = format!(" \u{1F512} {} (containerized) ", agent_name);

    let right_title = build_stats_title(tab);

    let mut block = Block::default()
        .title(Line::from(left_title).alignment(Alignment::Left))
        .title(Line::from(right_title).alignment(Alignment::Right))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green));

    // Probe the actual scrollback depth (only when scrolled) and build the indicator.
    // We probe by clamping set_scrollback to usize::MAX and reading back the capped value.
    let (effective_scroll_offset, max_scrollback) = if tab.container_scroll_offset > 0 {
        if let Some(ref mut parser) = tab.vt100_parser {
            // Compute effective (clamped) offset.
            parser.set_scrollback(tab.container_scroll_offset);
            let eff = parser.screen().scrollback();
            // Probe total scrollback depth.
            parser.set_scrollback(usize::MAX);
            let max = parser.screen().scrollback();
            // Reset to live view before rendering.
            parser.set_scrollback(0);
            (eff, max)
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    // Show scroll indicator when viewing scrollback.
    if effective_scroll_offset > 0 {
        let scroll_hint = format!(
            " \u{2191} scrollback ({} / {} lines) ",
            effective_scroll_offset, max_scrollback
        );
        block = block.title(
            Line::from(Span::styled(scroll_hint, Style::default().fg(Color::Yellow)))
                .alignment(Alignment::Center),
        );
    }

    // Build selection range for highlight rendering (normalised so start <= end).
    let selection = match (tab.terminal_selection_start, tab.terminal_selection_end) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };

    // Show copy hint in bottom border when text is selected.
    // CMD+C is not supported: macOS terminal emulators intercept it before the app receives it.
    if selection.is_some() {
        block = block.title_bottom(
            Line::from(Span::styled(
                " CTRL-Y to copy/yank text ",
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    }

    let inner = block.inner(container_area);
    frame.render_widget(block, container_area);

    // Store the inner area so the mouse handler can map terminal coordinates to vt100 cells.
    tab.container_inner_area = Some(inner);

    // Render the vt100 terminal emulator screen into the inner area.
    if let Some(ref mut parser) = tab.vt100_parser {
        if effective_scroll_offset > 0 {
            parser.set_scrollback(effective_scroll_offset);
            render_vt100_screen_no_cursor(frame, parser.screen(), inner, selection);
            parser.set_scrollback(0);
        } else {
            render_vt100_screen(frame, parser.screen(), inner, selection);
        }
    }
}

/// Render a vt100 screen into a ratatui buffer area, preserving colors,
/// bold/italic/underline, and cursor position.
///
/// `selection` optionally specifies a text selection range as `(start, end)` where each
/// element is `(row, col)` in vt100 screen coordinates. Selected cells are highlighted
/// with `Modifier::REVERSED` (inverted colours), matching standard terminal selection style.
fn render_vt100_screen(
    frame: &mut Frame,
    screen: &vt100::Screen,
    area: Rect,
    selection: Option<((u16, u16), (u16, u16))>,
) {
    let buf = frame.buffer_mut();
    let rows = area.height as usize;
    let cols = area.width as usize;
    let screen_rows = screen.size().0 as usize;
    let screen_cols = screen.size().1 as usize;

    // Normalise selection so (sr, sc) is always before (er, ec).
    let norm_sel = selection.map(|(s, e)| {
        if s.0 < e.0 || (s.0 == e.0 && s.1 <= e.1) { (s, e) } else { (e, s) }
    });

    for row in 0..rows.min(screen_rows) {
        let mut col = 0;
        while col < cols.min(screen_cols) {
            let cell = screen.cell(row as u16, col as u16);
            let x = area.x + col as u16;
            let y = area.y + row as u16;

            if let Some(cell) = cell {
                let contents = cell.contents();
                let mut style = Style::default();
                style = style.fg(convert_vt100_color(cell.fgcolor()));
                style = style.bg(convert_vt100_color(cell.bgcolor()));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                // Apply selection highlight.
                if cell_in_selection(norm_sel, row as u16, col as u16) {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                if contents.is_empty() {
                    buf[(x, y)].set_symbol(" ").set_style(style);
                } else {
                    buf[(x, y)].set_symbol(&contents).set_style(style);
                }
            }

            col += 1;
        }
    }

    // Render cursor position.
    if !screen.hide_cursor() {
        let (cursor_row, cursor_col) = screen.cursor_position();
        let cx = area.x + cursor_col;
        let cy = area.y + cursor_row;
        if cx < area.x + area.width && cy < area.y + area.height {
            frame.set_cursor_position((cx, cy));
        }
    }
}

/// Render a vt100 screen into a ratatui buffer area without showing the cursor.
/// Used when viewing scrollback history.
///
/// `selection` works the same as in `render_vt100_screen`.
fn render_vt100_screen_no_cursor(
    frame: &mut Frame,
    screen: &vt100::Screen,
    area: Rect,
    selection: Option<((u16, u16), (u16, u16))>,
) {
    let buf = frame.buffer_mut();
    let rows = area.height as usize;
    let cols = area.width as usize;
    let screen_rows = screen.size().0 as usize;
    let screen_cols = screen.size().1 as usize;

    // Normalise selection so (sr, sc) is always before (er, ec).
    let norm_sel = selection.map(|(s, e)| {
        if s.0 < e.0 || (s.0 == e.0 && s.1 <= e.1) { (s, e) } else { (e, s) }
    });

    for row in 0..rows.min(screen_rows) {
        let mut col = 0;
        while col < cols.min(screen_cols) {
            let cell = screen.cell(row as u16, col as u16);
            let x = area.x + col as u16;
            let y = area.y + row as u16;

            if let Some(cell) = cell {
                let contents = cell.contents();
                let mut style = Style::default();
                style = style.fg(convert_vt100_color(cell.fgcolor()));
                style = style.bg(convert_vt100_color(cell.bgcolor()));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                // Apply selection highlight.
                if cell_in_selection(norm_sel, row as u16, col as u16) {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                if contents.is_empty() {
                    buf[(x, y)].set_symbol(" ").set_style(style);
                } else {
                    buf[(x, y)].set_symbol(&contents).set_style(style);
                }
            }

            col += 1;
        }
    }
}

/// Check whether a cell at `(row, col)` falls within a normalised selection range.
///
/// `norm_sel` must already be normalised so `start <= end` in row-major order.
/// Returns `false` when `norm_sel` is `None`.
#[inline]
fn cell_in_selection(norm_sel: Option<((u16, u16), (u16, u16))>, row: u16, col: u16) -> bool {
    let Some(((sr, sc), (er, ec))) = norm_sel else { return false };
    if row < sr || row > er {
        return false;
    }
    if row == sr && col < sc {
        return false;
    }
    if row == er && col > ec {
        return false;
    }
    true
}

/// Convert a vt100 color to a ratatui color.
fn convert_vt100_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

// --- Minimized container bar ---

fn draw_minimized_container_bar(frame: &mut Frame, tab: &TabState, area: Rect) {
    let agent_name = tab
        .container_info
        .as_ref()
        .map(|i| i.agent_display_name.as_str())
        .unwrap_or("Agent");
    let stats_title = build_stats_title(tab);

    let content = format!(
        "\u{1F512} {} | {}",
        agent_name,
        stats_title.trim()
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green));

    let para = Paragraph::new(Line::from(vec![Span::styled(
        format!(" {}", content),
        Style::default().fg(Color::Green),
    )]))
    .block(block);

    frame.render_widget(para, area);
}

// --- Container summary bar (after container exits) ---

fn draw_container_summary(frame: &mut Frame, summary: &LastContainerSummary, area: Rect) {
    let exit_text = if summary.exit_code == 0 {
        "exit 0".to_string()
    } else {
        format!("exit {}", summary.exit_code)
    };

    let content = format!(
        " {} | {} | avg {} | avg {} | {} | {}",
        summary.agent_display_name,
        summary.container_name,
        summary.avg_cpu,
        summary.avg_memory,
        summary.total_time,
        exit_text,
    );

    // Use a custom border set with dashed lines for the summary.
    let border_set = ratatui::symbols::border::Set {
        top_left: "╭",
        top_right: "╮",
        bottom_left: "╰",
        bottom_right: "╯",
        horizontal_top: "╌",
        horizontal_bottom: "╌",
        vertical_left: "┆",
        vertical_right: "┆",
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set)
        .border_style(Style::default().fg(Color::DarkGray));

    let color = if summary.exit_code == 0 {
        Color::DarkGray
    } else {
        Color::Red
    };

    let para = Paragraph::new(Line::from(vec![Span::styled(
        content,
        Style::default().fg(color),
    )]))
    .block(block);

    frame.render_widget(para, area);
}

/// Build the right-side title for the container window: "name | cpu | mem | time"
fn build_stats_title(tab: &TabState) -> String {
    let info = match &tab.container_info {
        Some(i) => i,
        None => return String::new(),
    };

    let elapsed = info.start_time.elapsed().as_secs();
    let time_str = crate::tui::state::format_duration(elapsed);

    if let Some(ref stats) = info.latest_stats {
        format!(
            " {} | {} | {} | {} ",
            stats.name, stats.cpu_percent, stats.memory, time_str
        )
    } else {
        format!(" {} | ... | ... | {} ", info.container_name, time_str)
    }
}

// --- Status / hint bar ---

fn draw_status_bar(frame: &mut Frame, tab: &TabState, area: Rect) {
    let spans: Vec<Span> = match (&tab.phase, &tab.focus, &tab.container_window) {
        // Container maximized + window focused: ctrl-m to toggle, ctrl-w for workflow controls.
        (ExecutionPhase::Running { .. }, Focus::ExecutionWindow, ContainerWindowState::Maximized) => {
            if tab.workflow.is_some() && tab.workflow_current_step.is_some() {
                vec![Span::styled(
                    " ctrl-m minimize  ·  ctrl-w workflow controls ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " ctrl-m minimize  ·  scroll ↕ history ",
                    Style::default().fg(Color::Yellow),
                )]
            }
        }

        // Container minimized + window focused: hints for scrolling + ctrl-m to restore.
        (ExecutionPhase::Running { .. }, Focus::ExecutionWindow, ContainerWindowState::Minimized) => {
            vec![Span::styled(
                " ↑/↓ scroll  ·  b/e jump  ·  ctrl-m restore container  ·  Esc deselect ",
                Style::default().fg(Color::DarkGray),
            )]
        }

        // Running + window selected (no container): Esc to deselect.
        (ExecutionPhase::Running { .. }, Focus::ExecutionWindow, ContainerWindowState::Hidden) => {
            vec![Span::styled(
                " Press Esc to deselect the window ",
                Style::default().fg(Color::Yellow),
            )]
        }

        // Running + command box: ↑ to focus the window; ctrl-w hint when workflow is running.
        (ExecutionPhase::Running { .. }, Focus::CommandBox, _) => {
            if tab.workflow.is_some() && tab.workflow_current_step.is_some() {
                vec![Span::styled(
                    " Press ctrl-w for workflow controls ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " Press ↑ to focus the window ",
                    Style::default().fg(Color::DarkGray),
                )]
            }
        }

        // Done + window selected: Esc to deselect; ↑/↓ to scroll; b/e to jump.
        (ExecutionPhase::Done { .. }, Focus::ExecutionWindow, _) => vec![Span::styled(
            " ↑/↓ scroll  ·  b/e jump  ·  Esc deselect ",
            Style::default().fg(Color::DarkGray),
        )],

        // Done + command box: ↑ to focus the window.
        (ExecutionPhase::Done { .. }, Focus::CommandBox, _) => vec![Span::styled(
            " Press ↑ to focus the window ",
            Style::default().fg(Color::DarkGray),
        )],

        // Error + window selected: exit code + Esc + scroll hint.
        (ExecutionPhase::Error { exit_code, .. }, Focus::ExecutionWindow, _) => vec![
            Span::styled(
                format!(" Exit code: {} ", exit_code),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ·  ↑/↓ scroll  ·  b/e jump  ·  Esc deselect ",
                Style::default().fg(Color::DarkGray),
            ),
        ],

        // Error + command box: exit code always visible + ↑ to focus.
        (ExecutionPhase::Error { exit_code, .. }, Focus::CommandBox, _) => vec![
            Span::styled(
                format!(" Exit code: {} ", exit_code),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ·  Press ↑ to focus the window ",
                Style::default().fg(Color::DarkGray),
            ),
        ],

        _ => vec![],
    };

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}

// --- Command input box ---

fn draw_command_box(frame: &mut Frame, tab: &TabState, area: Rect) {
    let is_running = matches!(tab.phase, ExecutionPhase::Running { .. });
    let is_active = tab.focus == Focus::CommandBox && !is_running;

    let border_color = if is_active { Color::Cyan } else { Color::DarkGray };

    let title = if is_active { " command " } else { " command (inactive) " };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let content = if is_running && tab.focus == Focus::CommandBox {
        // Blocked: show hint about creating new tab
        vec![Line::from(vec![Span::styled(
            "  Press Ctrl+T to run another command in a new tab",
            Style::default().fg(Color::DarkGray),
        )])]
    } else if let Some(ref err) = tab.input_error {
        vec![Line::from(vec![Span::styled(
            format!("  {}", err),
            Style::default().fg(Color::Red),
        )])]
    } else {
        let prefix = Span::styled("> ", Style::default().fg(Color::Cyan));
        let text = Span::raw(tab.input.replace('\n', "↵"));
        vec![Line::from(vec![prefix, text])]
    };

    let para = Paragraph::new(content).block(block);
    frame.render_widget(para, area);

    if is_active && tab.input_error.is_none() {
        let cursor_x = area.x + 1 + 2 + tab.cursor_col as u16;
        let cursor_y = area.y + 1;
        if cursor_x < area.x + area.width - 1 {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

// --- Autocomplete suggestions ---

fn draw_suggestions(frame: &mut Frame, tab: &TabState, area: Rect) {
    // Show autocomplete suggestions when the command box is focused and suggestions exist.
    if tab.focus == Focus::CommandBox && !tab.suggestions.is_empty() {
        let spans: Vec<Span> = tab
            .suggestions
            .iter()
            .enumerate()
            .flat_map(|(i, s)| {
                let sep = if i == 0 {
                    Span::raw("  ")
                } else {
                    Span::styled("  ·  ", Style::default().fg(Color::DarkGray))
                };
                vec![
                    sep,
                    Span::styled(s.as_str(), Style::default().fg(Color::Cyan)),
                ]
            })
            .collect();
        let para = Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
        return;
    }

    // Always show directory context below the command box.
    let para = if let Some(ref worktree_path) = tab.worktree_active_path {
        let path_str = worktree_path.to_string_lossy().into_owned();
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("Using Worktree: ", Style::default().fg(Color::Blue)),
            Span::styled(path_str, Style::default().fg(Color::DarkGray)),
        ]))
    } else {
        let cwd_str = tab.cwd.to_string_lossy().into_owned();
        Paragraph::new(Line::from(vec![
            Span::styled("  CWD: ", Style::default().fg(Color::DarkGray)),
            Span::styled(cwd_str, Style::default().fg(Color::DarkGray)),
        ]))
    };
    frame.render_widget(para, area);
}

// --- Modal dialogs ---

fn draw_dialog(frame: &mut Frame, tab: &TabState, area: Rect) {
    // Special dialogs that do their own rendering.
    if let Dialog::ConfigShow(state) = &tab.dialog {
        draw_config_dialog(frame, state, area);
        return;
    }
    if let Dialog::NewInterviewSummary { kind, title, work_item_number, summary, cursor_pos } = &tab.dialog {
        draw_interview_summary_dialog(frame, kind, title, *work_item_number, summary, *cursor_pos, area);
        return;
    }
    if let Dialog::WorkflowControlBoard { current_step, error } = &tab.dialog {
        let container_minimized = tab.container_window == ContainerWindowState::Minimized;
        let is_last_step = tab.is_last_workflow_step();
        let next_step_agent = tab.next_step_different_agent();
        draw_workflow_control_board(frame, area, current_step, error.as_deref(), container_minimized, is_last_step, next_step_agent.as_deref());
        return;
    }
    if let Dialog::WorkflowYoloCountdown { current_step } = &tab.dialog {
        // Timing is authoritative from tab.yolo_countdown_started_at.
        let fallback = std::time::Instant::now();
        let started_at = tab.yolo_countdown_started_at.as_ref().unwrap_or(&fallback);
        draw_workflow_yolo_countdown(frame, area, current_step, started_at, &crate::tui::state::YOLO_COUNTDOWN_DURATION);
        return;
    }
    if let Dialog::WorktreeCommitPrompt { branch, uncommitted_files, message, cursor_pos, .. } = &tab.dialog {
        draw_worktree_commit_prompt(frame, area, branch, uncommitted_files, message, *cursor_pos);
        return;
    }
    if let Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos, .. } = &tab.dialog {
        draw_worktree_pre_commit_message(frame, area, uncommitted_files, message, *cursor_pos);
        return;
    }
    if let Dialog::RemoteSessionPicker { sessions, selected, .. } = &tab.dialog {
        let max_allowed = ((area.width as usize * 80) / 100).max(20);
        let items: Vec<String> = sessions.iter().map(|s| {
            format_session_picker_row(&s.id, &s.workdir, max_allowed)
        }).collect();
        draw_remote_picker(frame, area, " Select Remote Session ", items, *selected);
        return;
    }
    if let Dialog::RemoteSavedDirPicker { dirs, selected, .. } = &tab.dialog {
        draw_remote_picker(frame, area, " Select Working Directory ", dirs.clone(), *selected);
        return;
    }
    if let Dialog::RemoteSessionKillPicker { sessions, selected, .. } = &tab.dialog {
        let max_allowed = ((area.width as usize * 80) / 100).max(20);
        let items: Vec<String> = sessions.iter().map(|s| {
            format_session_picker_row(&s.id, &s.workdir, max_allowed)
        }).collect();
        draw_remote_picker(frame, area, " Select Session to Kill ", items, *selected);
        return;
    }

    let (title, body) = match &tab.dialog {
        Dialog::CloseTabConfirm => (
            " Ctrl+C pressed ",
            " Press ctrl-c again to quit amux, ctrl-t to close the current tab, or esc to cancel  ".to_string(),
        ),
        Dialog::NewTabDirectory { input, remote_sessions, remote_selected_idx, focus_workdir } => {
            let mut body = format!(
                "  Working directory:\n  > {}{}\n",
                input,
                if *focus_workdir { "█" } else { "" },
            );

            // Render remote sessions section if remote is configured.
            if let Some(result) = remote_sessions {
                let host_label = crate::config::effective_remote_default_addr()
                    .map(|a| crate::tui::state::extract_display_host(&a))
                    .unwrap_or_else(|| "remote".to_string());
                body.push_str(&format!("\n  ─── Remote sessions ({}) ─────────────\n", host_label));

                match result {
                    Ok(sessions) => {
                        for (i, s) in sessions.iter().enumerate() {
                            let prefix = if !focus_workdir && *remote_selected_idx == Some(i) {
                                "  > "
                            } else {
                                "    "
                            };
                            let short_id = if s.id.len() > 8 { &s.id[..8] } else { &s.id };
                            body.push_str(&format!("{}{}  {}\n", prefix, short_id, s.workdir));
                        }
                        let create_idx = sessions.len();
                        let prefix = if !focus_workdir && *remote_selected_idx == Some(create_idx) {
                            "  > "
                        } else {
                            "    "
                        };
                        body.push_str(&format!("{}+ Create new remote session\n", prefix));
                    }
                    Err(e) => {
                        body.push_str(&format!("    ⚠ Could not reach remote: {}\n", e));
                    }
                }
            } else {
                // Still loading.
                if crate::config::effective_remote_default_addr().is_some() {
                    body.push_str("\n    Loading remote sessions…\n");
                }
            }

            body.push_str("\n  [Enter] confirm  [Esc] cancel  [↓] remote list  ");
            (" New Tab ", body)
        },
        Dialog::NewRemoteSession { dir_input, saved_dirs, saved_selected_idx, focus_input, creation_error, .. } => {
            let mut body = format!(
                "  Remote working directory:\n  > {}{}\n",
                dir_input,
                if *focus_input { "█" } else { "" },
            );
            if !saved_dirs.is_empty() {
                body.push_str("\n  Saved directories:\n");
                for (i, d) in saved_dirs.iter().enumerate() {
                    let prefix = if !focus_input && *saved_selected_idx == Some(i) {
                        "  > "
                    } else {
                        "    "
                    };
                    body.push_str(&format!("{}{}\n", prefix, d));
                }
            }
            if let Some(err) = creation_error {
                body.push_str(&format!("\n  ⚠ {}\n", err));
            }
            body.push_str("\n  [Enter] confirm  [Esc] back  [↑↓] navigate  ");
            (" New Remote Session ", body)
        },
        Dialog::QuitConfirm => (
            " Ctrl+C pressed ",
            " Press ctrl-c again to quit amux, or esc to cancel  ".to_string(),
        ),
        Dialog::WorkflowCancelConfirm => (
            " Cancel Workflow Execution ",
            "  Cancel workflow execution?\n\n  The running container will be killed and the\n  current step returned to Pending for resumption.\n\n  [y] cancel execution   [n / Esc] keep running  ".to_string(),
        ),
        Dialog::MountScope { git_root, cwd } => (
            " Mount Scope ",
            format!(
                "  Git root: {}\n  CWD:      {}\n\n  Mount Git root (r) or CWD only (c)? [r/c]  ",
                git_root.display(),
                cwd.display()
            ),
        ),
        Dialog::AgentAuth { agent, git_root } => (
            " Agent Credentials ",
            format!(
                "  Mount {} credentials into the container?\n  (saved for this repo: {})\n\n  [y/n]  ",
                agent,
                git_root.display()
            ),
        ),
        Dialog::NewKindSelect { .. } => (
            " New Work Item — Type ",
            "  Select work item type:\n\n  1) Feature\n  2) Bug\n  3) Task\n  4) Enhancement\n\n  [1/2/3/4 or Esc to cancel]  ".to_string(),
        ),
        Dialog::NewTitleInput { kind, title: item_title, .. } => (
            " New Work Item — Title ",
            format!(
                "  Type: {}\n\n  Enter title: {}\n\n  [Enter to confirm, Esc to cancel]  ",
                kind.as_str(),
                item_title
            ),
        ),
        Dialog::ClawsReadyHasForked => (
            " Claws Ready — Fork ",
            "  Have you already forked nanoclaw on GitHub?\n\n  1) Yes\n  2) No (fork first)\n\n  [1/2 or Esc to cancel]  ".to_string(),
        ),
        Dialog::ClawsReadyUsernameInput { username } => (
            " Claws Ready — GitHub Username ",
            format!(
                "  Enter your GitHub username (fork owner):\n\n  > {}\n\n  [Enter to confirm, Esc to cancel]  ",
                username
            ),
        ),
        Dialog::ClawsAuditConfirm => (
            " Claws Init — Agent Audit ",
            "  amux will launch your code agent inside the container to configure\n  \
nanoclaw for containerized networking.\n\n  \
Allow the agent to work (could take up to 15m). When it finishes,\n  \
run /setup in the same agent session — no need to reattach.\n  \
The container continues running after you close the session.\n\n  \
Press y or 1 to accept and launch the agent,\n  \
or n or 2 (or Esc) to cancel.  ".to_string(),
        ),
        Dialog::ClawsReadyDockerSocketWarning => (
            " Claws Ready — Docker Socket Warning ",
            "  The nanoclaw container will be mounted to the host\n  Docker socket (like --allow-docker).\n  This grants elevated access to Docker.\n\n  Accept Docker socket access? [1=yes/2=no]  ".to_string(),
        ),
        Dialog::ClawsReadyOfferRestartStopped { container_id, name, created } => (
            " Claws Ready — Restart Stopped Container ",
            format!(
                "  Found a stopped nanoclaw container:\n\n  Name:    {}\n  ID:      {}\n  Created: {}\n\n  Start this stopped container? [1=yes/2=no]  ",
                name,
                &container_id[..container_id.len().min(12)],
                created,
            ),
        ),
        Dialog::ClawsRestartFailedOfferFresh { container_id } => (
            " Claws Ready — Restart Failed ",
            format!(
                "  Failed to start container {}.\n  The bind-mount sources (e.g. claude.json) may have been\n  cleaned up since the container was created.\n\n  Delete this container and start a fresh one? [1=yes/2=no]  ",
                &container_id[..container_id.len().min(12)],
            ),
        ),
        Dialog::ClawsReadyOfferStart => (
            " Claws Ready — Run Fresh Container ",
            format!(
                "  Run a fresh '{}' container? [1=yes/2=no]  ",
                crate::commands::claws::NANOCLAW_CONTROLLER_NAME,
            ),
        ),
        Dialog::ClawsReadySudoConfirm { password } => (
            " Claws Ready — Sudo Password ",
            format!(
                "  Clone to {} failed: permission denied.\n  Enter your sudo password to retry with sudo.\n\n  Password: {}\n\n  [Enter to confirm, Esc to cancel]  ",
                crate::commands::claws::nanoclaw_path_str(),
                "*".repeat(password.len()),
            ),
        ),
        Dialog::WorkflowStepConfirm { completed_step, next_steps } => (
            " Workflow Step Complete ",
            format!(
                "  Step '{}' completed successfully.\n\n  Next step(s): {}\n\n  \
                 [Enter/y] Advance to next step  [q/n/Esc] Pause workflow  ",
                completed_step,
                if next_steps.is_empty() { "none".to_string() } else { next_steps.join(", ") }
            ),
        ),
        Dialog::WorkflowStepError { failed_step, error } => (
            " Workflow Step Failed ",
            format!(
                "  Step '{}' failed.\n  Error: {}\n\n  \
                 [r/1] Retry step  [q/n/Esc] Pause workflow  ",
                failed_step,
                if error.len() > 60 { &error[..60] } else { error.as_str() }
            ),
        ),
        Dialog::WorktreeMergePrompt { branch, had_error, .. } => (
            " Worktree: Merge or Discard? ",
            format!(
                "  Branch '{}' {}.\n\n  \
                 [m/y] Merge into current branch\n  \
                 [d]   Discard (delete branch + worktree)\n  \
                 [s/Esc] Keep worktree branch as-is  ",
                branch,
                if *had_error { "finished with errors" } else { "completed" }
            ),
        ),
        Dialog::WorktreeMergeConfirm { branch, .. } => (
            " Worktree: Confirm Merge ",
            format!(
                "  Squash-merge branch '{}' into the current branch?\n\n  \
                 [y/Enter] Proceed with merge\n  \
                 [n/Esc]   Cancel  ",
                branch,
            ),
        ),
        Dialog::WorktreeDeleteConfirm { branch, .. } => (
            " Worktree: Delete Branch & Worktree? ",
            format!(
                "  Delete worktree and branch '{}'?\n\n  \
                 [y/Enter] Yes, delete\n  \
                 [n/Esc]   No, keep worktree  ",
                branch,
            ),
        ),
        Dialog::WorktreePreCommitWarning { uncommitted_files } => {
            let max_shown = 8usize;
            let files_str = uncommitted_files
                .iter()
                .take(max_shown)
                .map(|f| format!("  {}", f))
                .collect::<Vec<_>>()
                .join("\n");
            let overflow = uncommitted_files.len().saturating_sub(max_shown);
            let overflow_str = if overflow > 0 {
                format!("\n  … and {} more", overflow)
            } else {
                String::new()
            };
            (
                " Worktree: Uncommitted Changes ",
                format!(
                    "  The current branch has uncommitted files that\n  will NOT be included in the new worktree:\n\n{}{}\n\n  \
                     [c] Commit files before creating worktree\n  \
                     [u] Use last commit (proceed without uncommitted files)\n  \
                     [a/Esc] Abort  ",
                    files_str,
                    overflow_str,
                ),
            )
        }
        Dialog::ReadyLegacyMigration { agent_name } => (
            " Ready — Legacy Layout Detected ",
            format!(
                "  Detected legacy single-file Dockerfile.dev layout.\n\
                 \n\
                 Migrating will:\n\
                   1. Back up Dockerfile.dev to Dockerfile.dev.bak\n\
                   2. Recreate Dockerfile.dev with a minimal project base\n\
                   3. Write .amux/Dockerfile.{} using the agent template\n\
                   4. Build both images\n\
                   5. Run the audit agent to restore project dependencies\n\
                 \n\
                 Migrate to modular Dockerfile layout? [y=migrate / n=keep existing]  ",
                agent_name
            ),
        ),
        Dialog::ReadyTemplateAuditConfirm => (
            " Ready — Run Audit? ",
            "  Dockerfile.dev matches the default project template and has not been customised.\n\
             \n\
             The audit agent will scan your project and update Dockerfile.dev to install\n\
             all tools needed to build, run, and test it.\n\
             \n\
             Launch the audit container now? [y=yes / n=skip]  ".to_string(),
        ),
        Dialog::InitAuditConfirm { .. } => (
            " Init — Agent Audit ",
            "  The agent audit container will scan your project and update Dockerfile.dev\n\
             to ensure all tools needed to build, run, and test your project are installed.\n\
             \n\
             Run the agent audit container after init? [y=yes / n=skip]  ".to_string(),
        ),
        Dialog::InitReplaceAspec { .. } => (
            " Init — Replace aspec ",
            "  An aspec folder already exists at this Git root.\n\
             \n\
             Replace existing aspec folder with fresh templates? [y=yes / n=keep existing]  ".to_string(),
        ),
        Dialog::InitWorkItemsConfirm { .. } => (
            " Init — Work Items ",
            "  Would you like to configure a work items directory?\n\
             \n\
             [y=yes / n=skip]  ".to_string(),
        ),
        Dialog::InitWorkItemsDirInput { input, .. } => (
            " Init — Work Items Directory ",
            format!(
                "  Work items directory path (relative to repo root):\n\
                 \n\
                 > {}\n\
                 \n\
                 [Enter=confirm / Esc=skip]  ",
                input
            ),
        ),
        Dialog::InitWorkItemsTemplateInput { input, .. } => (
            " Init — Work Item Template (optional) ",
            format!(
                "  Work item template path (leave blank to skip):\n\
                 \n\
                 > {}\n\
                 \n\
                 [Enter=confirm / Esc=skip template]  ",
                input
            ),
        ),
        Dialog::AgentSetupConfirm { agent, default_agent, from_workflow, image_only } => (
            " Agent Setup Required ",
            if !from_workflow {
                if *image_only {
                    format!(
                        "  The '{}' agent image is not built.

  \n                         Build the agent image now?

  \n                         [y/Enter] Yes, build
  \n                         [n/Esc]   No, cancel  ",
                        agent
                    )
                } else {
                    format!(
                        "  The '{}' agent is not set up. Its Dockerfile is not present.

  \n                         Download the Dockerfile template from GitHub and build the agent image?

  \n                         [y/Enter] Yes, download and build
  \n                         [n/Esc]   No, cancel  ",
                        agent
                    )
                }
            } else if agent != default_agent {
                if *image_only {
                    format!(
                        "  Workflow step requires the '{}' agent, but its image is not built.

  \n                         Build the agent image now?

  \n                         [y/Enter] Yes, build
  \n                         [f]       Use '{}' instead (default agent)
  \n                         [n/Esc]   No, cancel workflow  ",
                        agent, default_agent
                    )
                } else {
                    format!(
                        "  Workflow step requires the '{}' agent, but its Dockerfile is not present.

  \n                         Download the Dockerfile template from GitHub and build the agent image?

  \n                         [y/Enter] Yes, download and build
  \n                         [f]       Use '{}' instead (default agent)
  \n                         [n/Esc]   No, cancel workflow  ",
                        agent, default_agent
                    )
                }
            } else {
                if *image_only {
                    format!(
                        "  Workflow step requires the '{}' agent, but its image is not built.

  \n                         Build the agent image now?

  \n                         [y/Enter] Yes, build
  \n                         [n/Esc]   No, cancel workflow  ",
                        agent
                    )
                } else {
                    format!(
                        "  Workflow step requires the '{}' agent, but its Dockerfile is not present.

  \n                         Download the Dockerfile template from GitHub and build the agent image?

  \n                         [y/Enter] Yes, download and build
  \n                         [n/Esc]   No, cancel workflow  ",
                        agent
                    )
                }
            },
        ),
        Dialog::None => return,
        // NewInterviewSummary is handled by the early return above — this arm is unreachable.
        Dialog::NewInterviewSummary { .. } => return,
        Dialog::WorkflowControlBoard { .. } => {
            // Handled by the special-case early return above — unreachable here.
            return;
        }
        Dialog::WorkflowYoloCountdown { .. } => {
            // Handled by the special-case early return above — unreachable here.
            return;
        }
        // WorktreeCommitPrompt is handled by the early return above — unreachable here.
        Dialog::WorktreeCommitPrompt { .. } => return,
        // WorktreePreCommitMessage is handled by the early return above — unreachable here.
        Dialog::WorktreePreCommitMessage { .. } => return,
        // ConfigShow is handled by the early return above — unreachable here.
        Dialog::ConfigShow { .. } => return,
        // Remote pickers are handled by early returns above — unreachable here.
        Dialog::RemoteSessionPicker { .. } => return,
        Dialog::RemoteSavedDirPicker { .. } => return,
        Dialog::RemoteSessionKillPicker { .. } => return,
        Dialog::RemoteSaveDirConfirm { dir, .. } => (
            " Save Directory? ",
            format!(
                "  Save '{}' to remote.savedDirs for future use?\n\n  [y]  Yes\n  [n/Esc]  No  ",
                dir
            ),
        ),
    };

    let popup_width = 72u16.min(area.width.saturating_sub(4));
    // Height = line count + 2 border rows, capped to terminal height.
    let line_count: u16 = body.chars().filter(|&c| c == '\n').count() as u16 + 1;
    let popup_height = (line_count + 2).max(5).min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let para = Paragraph::new(body.as_str())
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(para, popup);
}

fn draw_interview_summary_dialog(
    frame: &mut Frame,
    kind: &crate::commands::new::WorkItemKind,
    title: &str,
    work_item_number: u32,
    summary: &str,
    cursor_pos: usize,
    area: Rect,
) {
    // Compute popup size: 80% width, 60% height, min 16 rows
    let popup_width = ((area.width as u32 * 80 / 100) as u16).min(82).max(40);
    let popup_height = ((area.height as u32 * 60 / 100) as u16)
        .max(16)
        .min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    // Outer block
    let outer_block = Block::default()
        .title(" Interview Summary ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    if inner.height < 6 {
        return;
    }

    // Header rows: info + blank + instructions + blank
    let header_height = 4u16;
    let footer_height = 1u16;
    let text_area_height = inner.height.saturating_sub(header_height + footer_height);

    // Layout inside the popup inner area
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(text_area_height.max(3)),
            Constraint::Length(footer_height),
        ])
        .split(inner);

    let header_area = layout[0];
    let text_outer_area = layout[1];
    let footer_area = layout[2];

    // Render header
    let header_text = vec![
        Line::from(vec![Span::styled(
            format!("  {:04} — {}: {}", work_item_number, kind.as_str(), title),
            Style::default().fg(Color::Cyan),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Describe the work item. The code agent will complete the details.",
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(""),
    ];
    let header_para = Paragraph::new(header_text);
    frame.render_widget(header_para, header_area);

    // Render text area with border
    let text_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White));
    let text_inner = text_block.inner(text_outer_area);
    frame.render_widget(text_block, text_outer_area);

    // Split summary into logical lines
    let lines: Vec<&str> = summary.split('\n').collect();

    // Compute cursor logical row and col (in chars)
    let before_cursor = &summary[..cursor_pos.min(summary.len())];
    let cursor_logical_row = before_cursor.chars().filter(|&c| c == '\n').count();
    let last_newline_byte = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let cursor_logical_col = before_cursor[last_newline_byte..].chars().count();

    // Text width for wrapping (-1 for the " " prefix)
    let text_width = (text_inner.width.saturating_sub(1) as usize).max(1);

    // Build visual lines and find cursor visual position
    let mut all_visual_lines: Vec<String> = Vec::new();
    let mut cursor_visual_row = 0usize;
    let mut cursor_visual_col = 0usize;

    for (logical_row, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let line_char_len = chars.len();
        let num_visual = if line_char_len == 0 { 1 } else { (line_char_len + text_width - 1) / text_width };

        if logical_row == cursor_logical_row {
            cursor_visual_row = all_visual_lines.len() + cursor_logical_col / text_width;
            cursor_visual_col = cursor_logical_col % text_width;
        }

        for chunk_idx in 0..num_visual {
            let start = chunk_idx * text_width;
            let end = (start + text_width).min(line_char_len);
            let chunk: String = chars[start..end].iter().collect();
            all_visual_lines.push(chunk);
        }
    }

    // Scroll to keep cursor visible
    let visible_rows = text_inner.height as usize;
    let scroll_start = if cursor_visual_row >= visible_rows {
        cursor_visual_row + 1 - visible_rows
    } else {
        0
    };

    // Render visible visual lines
    let visible_lines: Vec<Line> = all_visual_lines
        .iter()
        .skip(scroll_start)
        .take(visible_rows)
        .map(|line| Line::from(format!(" {}", line)))
        .collect();

    let text_para = Paragraph::new(visible_lines);
    frame.render_widget(text_para, text_inner);

    // Place cursor
    let cursor_visible_row = cursor_visual_row.saturating_sub(scroll_start);
    let cx = text_inner.x + 1 + cursor_visual_col as u16; // +1 for the " " prefix
    let cy = text_inner.y + cursor_visible_row as u16;
    if cx < text_inner.x + text_inner.width && cy < text_inner.y + text_inner.height {
        frame.set_cursor_position((cx, cy));
    }

    // Render footer
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("  Ctrl+S / Ctrl+Enter to submit", Style::default().fg(Color::Green)),
        Span::raw("  ·  "),
        Span::styled("Esc to cancel", Style::default().fg(Color::DarkGray)),
    ]));
    frame.render_widget(footer, footer_area);
}

fn draw_worktree_commit_prompt(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    uncommitted_files: &[String],
    message: &str,
    cursor_pos: usize,
) {
    // Size: 80% width, enough height for header + file list + input box + footer
    let max_files_shown = 8usize;
    let files_shown = uncommitted_files.len().min(max_files_shown);
    let overflow = uncommitted_files.len().saturating_sub(max_files_shown);
    // header(3) + files + overflow(0..1) + blank(1) + input block(3) + footer(1) + borders(2)
    let extra = if overflow > 0 { 1 } else { 0 };
    let needed_h = (3 + files_shown + extra + 1 + 3 + 1 + 2) as u16;
    let popup_width = ((area.width as u32 * 80 / 100) as u16).min(82).max(50);
    let popup_height = needed_h.max(14).min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .title(" Worktree: Commit Uncommitted Files ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    if inner.height < 6 {
        return;
    }

    let footer_height = 1u16;
    let input_block_height = 3u16;
    let header_height = (3 + files_shown as u16 + extra as u16 + 1).max(3);
    let header_height = header_height.min(inner.height.saturating_sub(input_block_height + footer_height));

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(input_block_height),
            Constraint::Length(footer_height),
        ])
        .split(inner);

    let header_area = layout[0];
    let input_area = layout[1];
    let footer_area = layout[2];

    // Header: branch name + uncommitted files list
    let mut header_lines = vec![
        Line::from(vec![
            Span::raw("  Branch "),
            Span::styled(branch, Style::default().fg(Color::Cyan)),
            Span::raw(" has uncommitted files:"),
        ]),
        Line::from(""),
    ];
    for f in uncommitted_files.iter().take(max_files_shown) {
        header_lines.push(Line::from(vec![
            Span::styled(format!("  {}", f), Style::default().fg(Color::Yellow)),
        ]));
    }
    if overflow > 0 {
        header_lines.push(Line::from(vec![
            Span::styled(
                format!("  … and {} more", overflow),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    header_lines.push(Line::from(""));
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Input box: render text with cursor
    let input_block = Block::default()
        .title(" Commit message ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White));
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);

    // Build display string with a block-cursor character inserted at cursor_pos
    let cursor_char_pos = message[..cursor_pos.min(message.len())].chars().count();
    let chars: Vec<char> = message.chars().collect();
    let before: String = chars[..cursor_char_pos].iter().collect();
    let cursor_ch = chars.get(cursor_char_pos).copied().unwrap_or(' ');
    let after: String = chars[cursor_char_pos + if chars.get(cursor_char_pos).is_some() { 1 } else { 0 }..].iter().collect();

    let spans = vec![
        Span::raw(format!(" {}", before)),
        Span::styled(
            cursor_ch.to_string(),
            Style::default().bg(Color::White).fg(Color::Black),
        ),
        Span::raw(after),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), input_inner);

    // Footer
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("Ctrl+Enter", Style::default().fg(Color::Green)),
        Span::raw(" / "),
        Span::styled("Ctrl+S", Style::default().fg(Color::Green)),
        Span::raw(" to commit  ·  "),
        Span::styled("Esc", Style::default().fg(Color::DarkGray)),
        Span::raw(" to cancel"),
    ]));
    frame.render_widget(footer, footer_area);
}

fn draw_worktree_pre_commit_message(
    frame: &mut Frame,
    area: Rect,
    uncommitted_files: &[String],
    message: &str,
    cursor_pos: usize,
) {
    let max_files_shown = 8usize;
    let files_shown = uncommitted_files.len().min(max_files_shown);
    let overflow = uncommitted_files.len().saturating_sub(max_files_shown);
    let extra = if overflow > 0 { 1 } else { 0 };
    let needed_h = (3 + files_shown + extra + 1 + 3 + 1 + 2) as u16;
    let popup_width = ((area.width as u32 * 80 / 100) as u16).min(82).max(50);
    let popup_height = needed_h.max(14).min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let outer_block = Block::default()
        .title(" Commit Changes Before Creating Worktree ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    if inner.height < 6 {
        return;
    }

    let footer_height = 1u16;
    let input_block_height = 3u16;
    let header_height = (3 + files_shown as u16 + extra as u16 + 1).max(3);
    let header_height = header_height.min(inner.height.saturating_sub(input_block_height + footer_height));

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(input_block_height),
            Constraint::Length(footer_height),
        ])
        .split(inner);

    let header_area = layout[0];
    let input_area = layout[1];
    let footer_area = layout[2];

    let mut header_lines = vec![
        Line::from(vec![
            Span::raw("  The current branch has uncommitted files:"),
        ]),
        Line::from(""),
    ];
    for f in uncommitted_files.iter().take(max_files_shown) {
        header_lines.push(Line::from(vec![
            Span::styled(format!("  {}", f), Style::default().fg(Color::Yellow)),
        ]));
    }
    if overflow > 0 {
        header_lines.push(Line::from(vec![
            Span::styled(
                format!("  … and {} more", overflow),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    header_lines.push(Line::from(""));
    frame.render_widget(Paragraph::new(header_lines), header_area);

    let input_block = Block::default()
        .title(" Commit message ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White));
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);

    let cursor_char_pos = message[..cursor_pos.min(message.len())].chars().count();
    let chars: Vec<char> = message.chars().collect();
    let before: String = chars[..cursor_char_pos].iter().collect();
    let cursor_ch = chars.get(cursor_char_pos).copied().unwrap_or(' ');
    let after: String = chars[cursor_char_pos + if chars.get(cursor_char_pos).is_some() { 1 } else { 0 }..].iter().collect();

    let spans = vec![
        Span::raw(format!(" {}", before)),
        Span::styled(
            cursor_ch.to_string(),
            Style::default().bg(Color::White).fg(Color::Black),
        ),
        Span::raw(after),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), input_inner);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("Ctrl+Enter", Style::default().fg(Color::Green)),
        Span::raw(" / "),
        Span::styled("Ctrl+S", Style::default().fg(Color::Green)),
        Span::raw(" to commit  ·  "),
        Span::styled("Esc", Style::default().fg(Color::DarkGray)),
        Span::raw(" back"),
    ]));
    frame.render_widget(footer, footer_area);
}

fn draw_workflow_control_board(frame: &mut Frame, area: Rect, step_name: &str, error: Option<&str>, container_minimized: bool, is_last_step: bool, next_step_agent: Option<&str>) {
    // When the next step requires a different agent, "same container" is unavailable.
    let same_container_blocked = next_step_agent.is_some() && !is_last_step;
    let popup_width = 52u16.min(area.width.saturating_sub(4));
    // base_height accounts for 8 content lines + 1 cancel line + 1 hint + 2 border rows.
    // Last step adds 2 more lines for the Ctrl+Enter finish action.
    let base_height: u16 = if is_last_step { 15 } else { 13 };
    let popup_height = (if error.is_some() { base_height + 2 } else { base_height }).min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Workflow Control ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let arrow_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let label_style = Style::default();
    let dimmed_style = Style::default().fg(Color::DarkGray);
    let step_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let error_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let hint_style = Style::default().fg(Color::DarkGray);
    let finish_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);

    // Truncate step name if too long.
    let max_step_len = popup_width.saturating_sub(10) as usize;
    let step_display = if step_name.len() > max_step_len {
        format!("{}…", &step_name[..max_step_len.saturating_sub(1)])
    } else {
        step_name.to_string()
    };

    // Right arrow is dimmed and inactive on the last step.
    let (right_arrow_style, right_label_style) = if is_last_step {
        (dimmed_style, dimmed_style)
    } else {
        (arrow_style, label_style)
    };

    // Down arrow is dimmed when on the last step or when the next step requires a different agent.
    let (down_arrow_style, down_label_style) = if is_last_step || same_container_blocked {
        (dimmed_style, dimmed_style)
    } else {
        (arrow_style, label_style)
    };

    // The line after the down arrow: blank normally, or a note explaining the block.
    let down_note_line = if same_container_blocked {
        if let Some(agent) = next_step_agent {
            Line::from(vec![Span::styled(
                format!("           next step uses agent '{}'", agent),
                dimmed_style,
            )])
        } else {
            Line::raw("")
        }
    } else {
        Line::raw("")
    };

    let cancel_style = Style::default().fg(Color::Red);

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw(" Step: "),
            Span::styled(step_display, step_style),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("         "),
            Span::styled("↑", arrow_style),
            Span::styled(" Restart current step", label_style),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("←", arrow_style),
            Span::styled(" Cancel to prev", label_style),
            Span::raw("   "),
            Span::styled("→", right_arrow_style),
            Span::styled(" Next: new container", right_label_style),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("         "),
            Span::styled("↓", down_arrow_style),
            Span::styled(" Next: same container", down_label_style),
        ]),
        down_note_line,
        Line::from(vec![
            Span::raw("         "),
            Span::styled("^C", cancel_style),
            Span::styled(" Cancel workflow execution", cancel_style),
        ]),
    ];

    if is_last_step {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⏎", finish_style),
            Span::styled(" Ctrl+Enter  Finish workflow", finish_style),
        ]));
        lines.push(Line::raw(""));
    }

    if let Some(err) = error {
        lines.push(Line::from(vec![Span::styled(
            format!(" {}", err),
            error_style,
        )]));
        lines.push(Line::raw(""));
    }

    let hint = if is_last_step {
        if container_minimized {
            " [↑←] select  [Ctrl+Enter] finish  [c] restore  [^C] cancel  [d]isable  [Esc] dismiss"
        } else {
            " [↑←] select  [Ctrl+Enter] finish  [^C] cancel  [d]isable auto-popup  [Esc] dismiss"
        }
    } else if container_minimized {
        " [Arrow] select  [c] restore container  [^C] cancel  [d]isable auto-popup  [Esc] dismiss"
    } else {
        " [Arrow] select  [^C] cancel  [d]isable auto-popup for this step  [Esc] dismiss"
    };
    lines.push(Line::from(vec![Span::styled(hint, hint_style)]));

    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}

/// Render the yolo-mode countdown dialog shown when a workflow step is stuck.
fn draw_workflow_yolo_countdown(
    frame: &mut Frame,
    area: Rect,
    step_name: &str,
    started_at: &std::time::Instant,
    duration: &std::time::Duration,
) {
    let popup_width = 52u16.min(area.width.saturating_sub(4));
    let popup_height = 9u16.min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Yolo Mode — Auto-Advancing ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let elapsed = started_at.elapsed();
    let remaining = duration.saturating_sub(elapsed);
    let secs_remaining = remaining.as_secs();

    let step_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let countdown_style = Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD);
    let msg_style = Style::default().fg(Color::Yellow);
    let hint_style = Style::default().fg(Color::DarkGray);

    // Truncate step name if too long.
    let max_step_len = popup_width.saturating_sub(10) as usize;
    let step_display = if step_name.len() > max_step_len {
        format!("{}…", &step_name[..max_step_len.saturating_sub(1)])
    } else {
        step_name.to_string()
    };

    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw(" Step: "),
            Span::styled(step_display, step_style),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            format!(" No activity detected. Advancing to next step in {}s...", secs_remaining),
            msg_style,
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{}", secs_remaining), countdown_style),
            Span::styled(" seconds remaining", countdown_style),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(" [Esc] dismiss (60s backoff)", hint_style)]),
    ];

    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
}

/// Compute the popup dialog width for a remote picker.
///
/// The width is capped at 80% of the terminal width (at least 20 columns) so that
/// the dialog never dominates the entire screen even with very long session paths.
///
/// Formula:
/// - `max_allowed` = max(80% of `area_width`, 20)
/// - `content_width` = max(widest item in Unicode chars, title chars) + 4 (borders + padding)
/// - returns `content_width.min(max_allowed)`
pub(crate) fn popup_width_for(area_width: u16, items: &[String], title: &str) -> u16 {
    let max_allowed = ((area_width as usize * 80) / 100).max(20);
    let content_width = items
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(20)
        .max(title.chars().count())
        + 4; // 2 border + 2 padding each side
    content_width.min(max_allowed) as u16
}

/// Format a session picker row string, truncating the session ID if it would cause
/// the row to exceed `max_row_width` characters.  The workdir is never truncated
/// so the user can still identify which project the session belongs to.
pub(crate) fn format_session_picker_row(id: &str, workdir: &str, max_row_width: usize) -> String {
    let workdir_chars = workdir.chars().count();
    // "  (" = 3 chars, ")" = 1 char, surrounding spaces = 2 → 6 chars overhead
    let max_id_chars = max_row_width.saturating_sub(workdir_chars + 6);
    let id_display = if id.chars().count() > max_id_chars && max_id_chars > 3 {
        let truncated: String = id.chars().take(max_id_chars.saturating_sub(1)).collect();
        format!("{}…", truncated)
    } else {
        id.to_string()
    };
    format!("{}  ({})", id_display, workdir)
}

/// Draw a simple list picker dialog (used for remote session/dir pickers).
fn draw_remote_picker(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<String>,
    selected: usize,
) {
    let max_items = (area.height as usize).saturating_sub(6).max(1);
    let visible_items = items.len().min(max_items);
    let popup_height = (visible_items + 4).min(area.height as usize - 2) as u16;
    // Dynamic width: fit the widest item + padding, capped at 80% of terminal width.
    let popup_width = popup_width_for(area.width, &items, title);
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 2 {
        return;
    }

    // Show items with a scroll window centered on selected.
    let scroll_start = if selected >= visible_items {
        selected - visible_items + 1
    } else {
        0
    };

    let rows: Vec<Row> = items
        .iter()
        .enumerate()
        .skip(scroll_start)
        .take(visible_items)
        .map(|(i, item)| {
            let style = if i == selected {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default()
            };
            Row::new(vec![Cell::from(item.as_str()).style(style)])
        })
        .collect();

    let hint_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    let list_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };

    let table = Table::new(rows, [Constraint::Percentage(100)])
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(table, list_area);

    let hint = Paragraph::new("  ↑/↓ navigate   Enter select   Esc cancel")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, hint_area);
}

/// Return a centered rectangle of the given size within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect { x, y, width: width.min(area.width), height: height.min(area.height) }
}

// ─── Workflow strip ───────────────────────────────────────────────────────────

/// Compute the height in rows needed for the workflow status strip.
/// Returns 0 if there are no parallel groups (sequential only = 1 box row = 3 rows),
/// and up to 3 box-rows tall (9 rows max) for parallel groups.
pub fn workflow_strip_height(wf: &WorkflowState) -> u16 {
    let max_parallel = max_parallel_group_size(wf);
    // Cap at 3 box-rows. Each box is 3 rows tall.
    let rows = max_parallel.min(3) as u16;
    rows * 3
}

/// Return the size of the largest parallel group (steps sharing same depends_on set).
fn max_parallel_group_size(wf: &WorkflowState) -> usize {
    use std::collections::HashMap;
    let mut group_sizes: HashMap<Vec<String>, usize> = HashMap::new();
    for step in &wf.steps {
        let mut key = step.depends_on.clone();
        key.sort();
        *group_sizes.entry(key).or_insert(0) += 1;
    }
    group_sizes.values().copied().max().unwrap_or(1)
}

/// Render the workflow status strip.
///
/// The strip shows steps arranged left-to-right in topological order.
/// Parallel steps (same depends_on) are stacked vertically (max 3 rows), with
/// subsequent ones indented slightly to show they'll run sequentially.
pub fn draw_workflow_strip(
    frame: &mut Frame,
    wf: &WorkflowState,
    current_step: Option<&str>,
    area: Rect,
) {
    // Layout: one column per topological "level" (unique depends_on set in order).
    // Build ordered columns.
    let columns = build_workflow_columns(wf);
    if columns.is_empty() {
        return;
    }

    let num_cols = columns.len() as u16;
    // Distribute all available width across columns.
    // Reserve 1 char per inter-column arrow (N-1 arrows for N columns).
    // Each column box gets an equal share of the remaining space; the last
    // column absorbs any remainder so the strip always fills the full width.
    let arrow_chars = num_cols.saturating_sub(1);
    let box_space = area.width.saturating_sub(arrow_chars);
    let base_col_w = (box_space / num_cols).max(4);
    // Stride = box width + 1 arrow char between columns.
    let col_stride = base_col_w + 1;

    // Render each column.
    for (col_idx, col_steps) in columns.iter().enumerate() {
        let col_x = area.x + col_idx as u16 * col_stride;
        if col_x >= area.x + area.width {
            break; // Out of space — stop rendering columns.
        }

        // Last column gets any remaining space so the strip fills the full width.
        let this_col_w = if col_idx + 1 == columns.len() {
            (area.x + area.width).saturating_sub(col_x)
        } else {
            base_col_w
        };

        let visible_rows = (area.height / 3).max(1) as usize;
        let steps_to_show: Vec<_> = col_steps.iter().take(visible_rows).collect();
        let hidden_count = col_steps.len().saturating_sub(visible_rows);

        for (row_idx, step_name) in steps_to_show.iter().enumerate() {
            let row_y = area.y + row_idx as u16 * 3;
            if row_y + 3 > area.y + area.height {
                break;
            }

            // Indent by row_idx if there are multiple steps in column (parallel group).
            let indent = if col_steps.len() > 1 { row_idx as u16 } else { 0 };
            let box_x = (col_x + indent).min(area.x + area.width.saturating_sub(4));
            let box_w = this_col_w.saturating_sub(indent).max(4);

            let step = wf.get_step(step_name).unwrap();
            let is_current = current_step == Some(step_name.as_str());

            let (label, style) = step_box_label_and_style(step_name, &step.status, is_current, box_w);

            let box_area = Rect {
                x: box_x,
                y: row_y,
                width: box_w,
                height: 3,
            };

            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style);

            let para = Paragraph::new(label)
                .block(block)
                .style(style);
            frame.render_widget(para, box_area);

            // Render arrow between columns (→) in the 1-char gap after the box.
            if col_idx + 1 < columns.len() && row_idx == 0 {
                let arrow_x = col_x + this_col_w; // immediately after the box area
                if arrow_x < area.x + area.width {
                    let arrow_area = Rect { x: arrow_x, y: row_y + 1, width: 1, height: 1 };
                    let arrow = Paragraph::new("→")
                        .style(Style::default().fg(Color::DarkGray));
                    frame.render_widget(arrow, arrow_area);
                }
            }
        }

        // Show "+ N more" if there are hidden steps.
        if hidden_count > 0 {
            let last_row = steps_to_show.len().saturating_sub(1);
            let row_y = area.y + last_row as u16 * 3;
            if row_y + 3 <= area.y + area.height {
                let indent = last_row as u16;
                let box_x = (col_x + indent).min(area.x + area.width.saturating_sub(4));
                let box_w = this_col_w.saturating_sub(indent).max(4);
                let box_area = Rect { x: box_x, y: row_y, width: box_w, height: 3 };
                let more_label = format!("+ {} more…", hidden_count);
                let para = Paragraph::new(more_label)
                    .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray)))
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(para, box_area);
            }
        }
    }
}

/// Build topological columns for the workflow:
/// each column is a Vec of step names that share the same `depends_on` set,
/// in file order.
fn build_workflow_columns(wf: &WorkflowState) -> Vec<Vec<String>> {
    use std::collections::HashMap;

    // Map each depends_on signature (sorted deps) → column index, preserving order.
    let mut sig_to_col: HashMap<Vec<String>, usize> = HashMap::new();
    let mut columns: Vec<Vec<String>> = Vec::new();

    // Process steps in topological order (preserved by file order for same-level steps).
    let topo = crate::workflow::dag::topological_order(
        &wf.steps.iter().map(|s| crate::workflow::parser::WorkflowStep {
            name: s.name.clone(),
            depends_on: s.depends_on.clone(),
            prompt_template: String::new(),
            agent: None,
            model: None,
        }).collect::<Vec<_>>(),
    );

    for step_name in &topo {
        let step = match wf.get_step(step_name) {
            Some(s) => s,
            None => continue,
        };
        let mut sig = step.depends_on.clone();
        sig.sort();
        let col_idx = *sig_to_col.entry(sig).or_insert_with(|| {
            columns.push(Vec::new());
            columns.len() - 1
        });
        columns[col_idx].push(step_name.clone());
    }

    columns
}

/// Return (label_text, style) for a step box.
fn step_box_label_and_style(
    name: &str,
    status: &StepStatus,
    is_current: bool,
    _box_width: u16,
) -> (String, Style) {
    const MAX_NAME: usize = 12;
    let truncated_name = if name.len() > MAX_NAME {
        format!("{}…", &name[..MAX_NAME.saturating_sub(1)])
    } else {
        name.to_string()
    };

    let (status_char, style) = match status {
        StepStatus::Pending => (
            "○",
            Style::default().fg(Color::DarkGray),
        ),
        StepStatus::Running => (
            "●",
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        StepStatus::Done => (
            "✓",
            Style::default().fg(Color::Green),
        ),
        StepStatus::Error(_) => (
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };

    let style = if is_current {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };

    // Fit label in box: " ● name "
    let label = format!(" {} {} ", status_char, truncated_name);
    (label, style)
}

// ── Config dialog ─────────────────────────────────────────────────────────────

fn draw_config_dialog(
    frame: &mut Frame,
    state: &crate::tui::state::ConfigDialogState,
    area: Rect,
) {
    use crate::commands::config::{
        ALL_FIELDS, effective_display, global_display, override_indicator, repo_display,
    };

    // Large centered popup — use most of the terminal.
    let popup_width = area.width.saturating_sub(4).min(110);
    let popup_height = area.height.saturating_sub(4).min(26);
    let popup = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" amux config ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // inner layout: table rows, separator, hint line, key hints
    // In edit mode we add one extra line to display the full current value (which
    // may be wider than the table column and would otherwise be clipped with "…").
    let bottom_height = match (state.edit_mode, state.error_msg.is_some()) {
        (true, true)  => 4u16,
        (true, false) | (false, true) => 3u16,
        (false, false) => 2u16,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(bottom_height)])
        .split(inner);

    let table_area = chunks[0];
    let hint_area = chunks[1];

    // Build header row.
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Field").style(header_style),
        Cell::from("Global").style(header_style),
        Cell::from("Repo").style(header_style),
        Cell::from("Effective").style(header_style),
        Cell::from("Override").style(header_style),
    ])
    .height(1)
    .bottom_margin(0);

    // Build data rows.
    let repo_opt = if state.git_root.is_some() { Some(&state.repo_config) } else { None };

    let rows: Vec<Row> = ALL_FIELDS.iter().enumerate().map(|(i, field)| {
        let is_selected = i == state.selected_row;

        let gval = if is_selected && state.edit_mode && state.selected_col == 0 {
            // Show inline edit cursor in Global column.
            let ev = &state.edit_value;
            let cursor = state.edit_cursor;
            format!("{}|{}", &ev[..cursor], &ev[cursor..])
        } else {
            global_display(field, &state.global_config)
        };

        let rval = if is_selected && state.edit_mode && state.selected_col == 1 {
            // Show inline edit cursor in Repo column.
            let ev = &state.edit_value;
            let cursor = state.edit_cursor;
            format!("{}|{}", &ev[..cursor], &ev[cursor..])
        } else {
            repo_display(field, repo_opt)
        };

        let ev = effective_display(field, &state.global_config, repo_opt);
        let ov = override_indicator(field, &state.global_config, repo_opt);

        // Highlight selected column within selected row.
        let (gcell, rcell) = if is_selected && !state.edit_mode {
            let col_style = Style::default().fg(Color::Black).bg(Color::White);
            if state.selected_col == 0 {
                (Cell::from(gval).style(col_style), Cell::from(rval))
            } else {
                (Cell::from(gval), Cell::from(rval).style(col_style))
            }
        } else if is_selected && state.edit_mode {
            let edit_style = Style::default().fg(Color::Black).bg(Color::Green);
            if state.selected_col == 0 {
                (Cell::from(gval).style(edit_style), Cell::from(rval))
            } else {
                (Cell::from(gval), Cell::from(rval).style(edit_style))
            }
        } else {
            (Cell::from(gval), Cell::from(rval))
        };

        let row = Row::new(vec![
            Cell::from(field.key),
            gcell,
            rcell,
            Cell::from(ev),
            Cell::from(ov),
        ]);

        if is_selected {
            row.style(Style::default().fg(Color::White).bg(Color::DarkGray))
        } else {
            row
        }
    }).collect();

    // Column widths (Percentage-based so they scale with popup width).
    let widths = [
        Constraint::Percentage(26),
        Constraint::Percentage(20),
        Constraint::Percentage(20),
        Constraint::Percentage(20),
        Constraint::Percentage(14),
    ];

    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, table_area);

    // Bottom hint area.
    let selected_field = &ALL_FIELDS[state.selected_row];
    let mut hint_lines: Vec<Line> = Vec::new();

    if let Some(ref err) = state.error_msg {
        hint_lines.push(Line::from(Span::styled(
            format!("Error: {}", err),
            Style::default().fg(Color::Red),
        )));
    }

    if state.edit_mode {
        // Show the full current edit value with cursor marker so that long values
        // are visible even when the table cell clips them with "…".
        let ev = &state.edit_value;
        let cursor = state.edit_cursor;
        let value_line = format!("  {}|{}", &ev[..cursor], &ev[cursor..]);
        hint_lines.push(Line::from(Span::styled(
            value_line,
            Style::default().fg(Color::Green),
        )));
        hint_lines.push(Line::from(Span::styled(
            format!("  Editing {}  |  Accepted: {}  |  Enter=save  Esc=cancel", selected_field.key, selected_field.hint),
            Style::default().fg(Color::Green),
        )));
    } else {
        hint_lines.push(Line::from(vec![
            Span::styled("  ↑↓", Style::default().fg(Color::Yellow)),
            Span::raw("=row  "),
            Span::styled("←→", Style::default().fg(Color::Yellow)),
            Span::raw("=col(Both fields)  "),
            Span::styled("e", Style::default().fg(Color::Yellow)),
            Span::raw("=edit  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw("=close  "),
            Span::styled("Hint:", Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {}", selected_field.hint)),
        ]));
    }

    let hint_para = Paragraph::new(hint_lines).wrap(Wrap { trim: false });
    frame.render_widget(hint_para, hint_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::App;
    use ratatui::{backend::TestBackend, Terminal};

    fn new_app() -> App {
        App::new(std::path::PathBuf::new())
    }

    /// Helper: render the app into a TestBackend and return the text content
    /// of the execution window's inner area (excluding borders).
    /// No sidebar. Tab bar is 3 rows at top. Exec window starts after tab bar.
    fn render_exec_window_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        // Tab bar takes 3 rows. Exec window height = total - 3 (tab bar) - 5 (status+cmd+suggest).
        let tab_bar_height = 3u16;
        let exec_height = height.saturating_sub(tab_bar_height + 5);
        let inner_top = tab_bar_height + 1; // after tab bar + top border
        let inner_left = 1u16;              // no sidebar, just left border
        let inner_width = width.saturating_sub(2);
        let inner_rows = exec_height.saturating_sub(2);

        let mut lines = Vec::new();
        for row in inner_top..(inner_top + inner_rows) {
            let mut line = String::new();
            for col in inner_left..(inner_left + inner_width) {
                let cell = &buf[(col, row)];
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    #[test]
    fn scroll_changes_visible_content_in_done_state() {
        let mut app = new_app();
        // Terminal: 40 wide, 18 tall
        // exec window = 18 - 3 (tab bar) - 5 (status+cmd+suggest) = 10 rows → inner = 8 rows
        // Add 20 lines of output so there's content to scroll through.
        for i in 0..20 {
            app.active_tab_mut().output_lines.push(format!("line {}", i));
        }
        app.active_tab_mut().phase = ExecutionPhase::Done {
            command: "ready".into(),
        };
        app.active_tab_mut().focus = Focus::ExecutionWindow;

        // scroll_offset=0 → should show the LAST 8 lines (lines 12-19).
        app.active_tab_mut().scroll_offset = 0;
        let view0 = render_exec_window_lines(&mut app, 40, 18);
        assert!(
            view0.iter().any(|l| l.contains("line 19")),
            "scroll_offset=0 should show line 19 (newest). Got: {:?}",
            view0
        );
        assert!(
            !view0.iter().any(|l| l.contains("line 0")),
            "scroll_offset=0 should NOT show line 0 (oldest). Got: {:?}",
            view0
        );

        // scroll_offset=5 → should show earlier content (lines 7-14 with 8 inner rows).
        app.active_tab_mut().scroll_offset = 5;
        let view5 = render_exec_window_lines(&mut app, 40, 18);
        assert!(
            view5.iter().any(|l| l.contains("line 8")),
            "scroll_offset=5 should show line 8. Got: {:?}",
            view5
        );

        // The two views must differ.
        assert_ne!(
            view0, view5,
            "Scrolling must change the visible content"
        );

        // scroll_offset=max → should show the FIRST lines.
        app.active_tab_mut().scroll_offset = 20;
        let view_top = render_exec_window_lines(&mut app, 40, 18);
        assert!(
            view_top.iter().any(|l| l.contains("line 0")),
            "scroll_offset=max should show line 0 (oldest). Got: {:?}",
            view_top
        );
    }

    #[test]
    fn unicode_lines_do_not_cause_scroll_overshoot() {
        let mut app = new_app();
        // Box-drawing chars: "─" is 3 bytes but 1 display column.
        for i in 0..10 {
            app.active_tab_mut().output_lines.push(format!("──── step {} ────", i));
        }
        app.active_tab_mut().phase = ExecutionPhase::Done {
            command: "ready".into(),
        };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().scroll_offset = 0;

        // 40 wide, 18 tall → exec_height = 9, inner = 7 rows.
        let view = render_exec_window_lines(&mut app, 40, 18);
        assert!(
            view.iter().any(|l| l.contains("step 9")),
            "Newest line must be visible with Unicode content. Got: {:?}",
            view
        );
    }

    #[test]
    fn container_summary_renders_after_container_exit() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Done { command: "implement 0001".into() };
        app.active_tab_mut().last_container_summary = Some(LastContainerSummary {
            agent_display_name: "Claude Code".into(),
            container_name: "amux-test".into(),
            avg_cpu: "5.0%".into(),
            avg_memory: "200MiB".into(),
            total_time: "12m".into(),
            exit_code: 0,
        });

        // Render with enough space to include the summary bar.
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        // Collect all text from the buffer to verify summary content appears.
        let mut all_text = String::new();
        for row in 0..20 {
            for col in 0..80 {
                let cell = &buf[(col, row)];
                all_text.push_str(cell.symbol());
            }
        }
        assert!(
            all_text.contains("Claude Code"),
            "Summary should contain agent name. Got buffer text."
        );
        assert!(
            all_text.contains("amux-test"),
            "Summary should contain container name."
        );
    }

    #[test]
    fn container_window_renders_when_maximized() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        // Use size matching what TestBackend(80,25) would produce.
        let (inner_cols, inner_rows) = calculate_container_inner_size(80, 25, 0);
        app.active_tab_mut().start_container("amux-test".into(), "Claude Code".into(), inner_cols, inner_rows);

        // Feed data through the vt100 parser.
        if let Some(ref mut parser) = app.active_tab_mut().vt100_parser {
            parser.process(b"Hello from container\r\n");
        }

        let backend = TestBackend::new(80, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        let mut all_text = String::new();
        for row in 0..25 {
            for col in 0..80 {
                let cell = &buf[(col, row)];
                all_text.push_str(cell.symbol());
            }
        }
        // Container window should show agent name and "containerized".
        assert!(
            all_text.contains("containerized"),
            "Container window should show '(containerized)' label"
        );
        // Container output should be visible via vt100 rendering.
        assert!(
            all_text.contains("Hello from container"),
            "Container output should be rendered in the container window"
        );
    }

    #[test]
    fn minimized_container_bar_renders() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().start_container("amux-test".into(), "Claude Code".into(), 78, 18);
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        let mut all_text = String::new();
        for row in 0..20 {
            for col in 0..80 {
                let cell = &buf[(col, row)];
                all_text.push_str(cell.symbol());
            }
        }
        assert!(
            all_text.contains("Claude Code"),
            "Minimized bar should contain agent name"
        );
    }

    #[test]
    fn calculate_container_inner_size_reasonable_values() {
        let (cols, rows) = calculate_container_inner_size(80, 25, 0);
        // exec_height = 25 - 3 (tab bar) - 5 (status+cmd+suggest) = 17
        // container_height = 17 * 95 / 100 = 16
        // container_width = 80 * 95 / 100 = 76
        // inner_rows = 16 - 2 = 14
        // inner_cols = 76 - 2 = 74
        assert_eq!(cols, 74);
        assert_eq!(rows, 14);
    }

    #[test]
    fn container_window_is_95_percent_and_centered() {
        // Verify the container window occupies 95% of content area and is centered.
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        let (inner_cols, inner_rows) = calculate_container_inner_size(100, 30, 0);
        app.active_tab_mut().start_container("test".into(), "Agent".into(), inner_cols, inner_rows);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        // No sidebar. exec_height = 30 - 4 (tab bar) - 5 = 21
        // container_width = 100 * 95/100 = 95, container_height = 21 * 95/100 = 19
        // offset_x = (100 - 95)/2 = 2, offset_y = (21 - 19)/2 = 1
        // abs_x = 0 + 2 = 2, abs_y = 4 (tab bar) + 1 (offset_y) = 5
        // Border at (2, 5)
        let corner = buf[(2, 5)].symbol().to_string();
        assert!(
            corner == "╭" || corner == "│" || corner == "─",
            "Container border character should appear at centered position. Got: '{}'",
            corner
        );
    }

    #[test]
    fn vt100_set_scrollback_basic() {
        // Verify basic vt100 set_scrollback behavior.
        let mut parser = vt100::Parser::new(5, 20, 100);
        for i in 0..20 {
            parser.process(format!("line {}\r\n", i).as_bytes());
        }
        // After 20 lines in a 5-row screen, 15 lines should be in scrollback.
        // scrollback() returns the current position (0 when normal view).
        assert_eq!(parser.screen().scrollback(), 0);

        parser.set_scrollback(5);
        assert_eq!(parser.screen().scrollback(), 5);
        // cell(0,0) should access scrollback content.
        let cell = parser.screen().cell(0, 0);
        assert!(cell.is_some(), "cell(0,0) should be valid with scrollback=5");

        parser.set_scrollback(0);
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn container_scrollback_renders_older_content() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        let (inner_cols, inner_rows) = calculate_container_inner_size(80, 25, 0);
        app.active_tab_mut().start_container("test".into(), "Agent".into(), inner_cols, inner_rows);

        // Feed enough data to create scrollback: write many lines to push
        // content into the scrollback buffer.
        if let Some(ref mut parser) = app.active_tab_mut().vt100_parser {
            for i in 0..50 {
                parser.process(format!("scrollback line {}\r\n", i).as_bytes());
            }
        }

        // At offset 0, the latest lines should be visible.
        app.active_tab_mut().container_scroll_offset = 0;
        let backend = TestBackend::new(80, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text_at_0 = String::new();
        for row in 0..25 {
            for col in 0..80 {
                text_at_0.push_str(buf[(col, row)].symbol());
            }
        }
        assert!(
            text_at_0.contains("scrollback line 49"),
            "At offset 0 the latest line should be visible"
        );

        // Scroll up by a safe amount (capped at screen rows = inner_rows).
        let max_safe = inner_rows as usize;
        app.active_tab_mut().container_scroll_offset = max_safe;
        let backend = TestBackend::new(80, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text_scrolled = String::new();
        for row in 0..25 {
            for col in 0..80 {
                text_scrolled.push_str(buf[(col, row)].symbol());
            }
        }
        // When scrolled max_safe lines up, the most recent line should not be visible.
        assert!(
            !text_scrolled.contains("scrollback line 49"),
            "At max scroll the latest line should NOT be visible"
        );
        // Should show earlier content from scrollback.
        assert!(
            text_scrolled.contains("scrollback line"),
            "Should show scrollback content when scrolled up"
        );
    }

    #[test]
    fn container_scroll_indicator_shown_when_scrolled() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        let (inner_cols, inner_rows) = calculate_container_inner_size(80, 25, 0);
        app.active_tab_mut().start_container("test".into(), "Agent".into(), inner_cols, inner_rows);

        // Feed data to create scrollback.
        if let Some(ref mut parser) = app.active_tab_mut().vt100_parser {
            for i in 0..50 {
                parser.process(format!("line {}\r\n", i).as_bytes());
            }
        }

        // Use a scroll offset within the safe range (≤ screen rows).
        app.active_tab_mut().container_scroll_offset = (inner_rows as usize).min(10);
        let backend = TestBackend::new(80, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut all_text = String::new();
        for row in 0..25 {
            for col in 0..80 {
                all_text.push_str(buf[(col, row)].symbol());
            }
        }
        assert!(
            all_text.contains("scrollback"),
            "Scroll indicator should appear when scrolled up. Got buffer text."
        );
    }

    #[test]
    fn outer_window_scroll_unaffected_by_container_changes() {
        // Verify that the outer execution window scrolling still works correctly
        // even when container-related state is present.
        let mut app = new_app();
        for i in 0..20 {
            app.active_tab_mut().output_lines.push(format!("outer line {}", i));
        }
        app.active_tab_mut().phase = ExecutionPhase::Done { command: "ready".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        // Container is hidden (default) — this should not affect outer scrolling.
        app.active_tab_mut().container_scroll_offset = 5; // stale value, should be irrelevant

        app.active_tab_mut().scroll_offset = 0;
        let view_bottom = render_exec_window_lines(&mut app, 40, 18);
        assert!(
            view_bottom.iter().any(|l| l.contains("outer line 19")),
            "Outer window should show newest line at offset 0. Got: {:?}",
            view_bottom
        );

        app.active_tab_mut().scroll_offset = 10;
        let view_scrolled = render_exec_window_lines(&mut app, 40, 18);
        assert!(
            !view_scrolled.iter().any(|l| l.contains("outer line 19")),
            "Outer window should not show newest line at offset 10. Got: {:?}",
            view_scrolled
        );
    }

    #[test]
    fn tab_bar_renders_at_top() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/myproject"));
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Top-left corner of the first tab's rounded border should be at (0, 0).
        let corner = buf[(0, 0)].symbol().to_string();
        assert!(
            corner == "╭" || corner == "─",
            "Tab bar border at (0,0): '{}'",
            corner
        );
        // Row 3 should be the start of the exec window border (tab bar is 3 rows).
        let exec_border = buf[(0, 3)].symbol().to_string();
        assert!(
            exec_border == "╭" || exec_border == "─" || exec_border == " ",
            "Exec window border or space should start at row 3. Got: '{}'",
            exec_border
        );
    }

    #[test]
    fn single_tab_renders_without_panic() {
        // With exactly one tab (active_tab_idx == 0), the tab bar must render
        // without any out-of-bounds access and the active tab must use the
        // open-bottom (no bottom border) style.
        let mut app = new_app();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab_idx, 0);

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        // Must not panic.
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        // The active tab occupies columns 0..20, rows 0..3.
        // With Borders::TOP | LEFT | RIGHT (no bottom), row 2 of the tab area
        // should NOT contain a horizontal border character at the bottom line.
        // Row 0 is the top border (╭ … ╮), row 2 is the bottom of the tab block.
        // For an active tab, the bottom row should be blank (space), not "─".
        let bottom_left = buf[(0, 2)].symbol().to_string();
        assert_ne!(
            bottom_left, "╰",
            "Active tab bottom-left corner should not appear (no bottom border). Got: '{}'",
            bottom_left
        );
        // The top border should still be present.
        let top_left = buf[(0, 0)].symbol().to_string();
        assert!(
            top_left == "╭" || top_left == "─",
            "Active tab top border should be present. Got: '{}'",
            top_left
        );
    }

    #[test]
    fn active_tab_has_no_bottom_border_inactive_tabs_do() {
        // With two tabs, the active tab suppresses its bottom border while the
        // inactive tab retains its full border (Borders::ALL).
        let mut app = new_app();
        // Add a second tab by pushing a new TabState.
        let second = crate::tui::state::TabState::new(std::path::PathBuf::new());
        app.tabs.push(second);
        assert_eq!(app.tabs.len(), 2);
        app.active_tab_idx = 0; // first tab is active

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        // Tab width is 20. Active tab: cols 0..20. Inactive tab: cols 20..40.
        // Row 2 is the bottom row of the 3-row tab area.
        // Active tab (col 0, row 2): no bottom border → should not be "╰".
        let active_bottom_left = buf[(0, 2)].symbol().to_string();
        assert_ne!(
            active_bottom_left, "╰",
            "Active tab must not have a bottom-left corner. Got: '{}'",
            active_bottom_left
        );

        // Inactive tab (col 20, row 2): should have a bottom border "╰".
        let inactive_bottom_left = buf[(20, 2)].symbol().to_string();
        assert_eq!(
            inactive_bottom_left, "╰",
            "Inactive tab must have a bottom-left corner. Got: '{}'",
            inactive_bottom_left
        );
    }

    #[test]
    fn workflow_strip_renders_without_panic() {
        use crate::workflow::{WorkflowState, WorkflowStepState, StepStatus};

        let mut app = new_app();

        // Build a minimal WorkflowState with three steps (plan → implement → review).
        let steps = vec![
            WorkflowStepState {
                name: "plan".to_string(),
                depends_on: vec![],
                prompt_template: "Plan the work.".to_string(),
                status: StepStatus::Done,
                container_id: None,
                agent: None,
                model: None,
            },
            WorkflowStepState {
                name: "implement".to_string(),
                depends_on: vec!["plan".to_string()],
                prompt_template: "Implement it.".to_string(),
                status: StepStatus::Running,
                container_id: Some("abc123".to_string()),
                agent: None,
                model: None,
            },
            WorkflowStepState {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string()],
                prompt_template: "Review the changes.".to_string(),
                status: StepStatus::Pending,
                container_id: None,
                agent: None,
                model: None,
            },
        ];

        let wf = WorkflowState {
            title: Some("Test Workflow".to_string()),
            steps,
            workflow_hash: "deadbeef".to_string(),
            work_item: Some(27),
            workflow_name: "test-workflow".to_string(),
        };

        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("implement".to_string());

        // Render — must not panic.
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        // Collect all rendered text.
        let buf = terminal.backend().buffer();
        let mut all_text = String::new();
        for row in 0..30 {
            for col in 0..80 {
                all_text.push_str(buf[(col, row)].symbol());
            }
        }

        // The workflow strip should contain at least one of the step names.
        assert!(
            all_text.contains("plan") || all_text.contains("impl") || all_text.contains("review"),
            "Workflow strip should render step names. Buffer text: {:?}",
            &all_text[..all_text.len().min(200)]
        );
    }

    #[test]
    fn workflow_strip_height_matches_parallel_groups() {
        use crate::workflow::{WorkflowState, WorkflowStepState, StepStatus};

        // Single-column workflow (linear chain): height should be 3 (1 row of boxes).
        let linear_steps = vec![
            WorkflowStepState {
                name: "a".to_string(),
                depends_on: vec![],
                prompt_template: String::new(),
                status: StepStatus::Pending,
                container_id: None,
                agent: None,
                model: None,
            },
            WorkflowStepState {
                name: "b".to_string(),
                depends_on: vec!["a".to_string()],
                prompt_template: String::new(),
                status: StepStatus::Pending,
                container_id: None,
                agent: None,
                model: None,
            },
        ];
        let wf_linear = WorkflowState {
            title: None,
            steps: linear_steps,
            workflow_hash: "h".to_string(),
            work_item: Some(1),
            workflow_name: "w".to_string(),
        };
        let h = workflow_strip_height(&wf_linear);
        assert!(h >= 3, "Strip height for linear workflow should be at least 3. Got: {}", h);

        // Parallel group (both b and c depend on a): height should accommodate stacking.
        let parallel_steps = vec![
            WorkflowStepState {
                name: "a".to_string(),
                depends_on: vec![],
                prompt_template: String::new(),
                status: StepStatus::Pending,
                container_id: None,
                agent: None,
                model: None,
            },
            WorkflowStepState {
                name: "b".to_string(),
                depends_on: vec!["a".to_string()],
                prompt_template: String::new(),
                status: StepStatus::Pending,
                container_id: None,
                agent: None,
                model: None,
            },
            WorkflowStepState {
                name: "c".to_string(),
                depends_on: vec!["a".to_string()],
                prompt_template: String::new(),
                status: StepStatus::Pending,
                container_id: None,
                agent: None,
                model: None,
            },
        ];
        let wf_parallel = WorkflowState {
            title: None,
            steps: parallel_steps,
            workflow_hash: "h".to_string(),
            work_item: Some(1),
            workflow_name: "w".to_string(),
        };
        let h_parallel = workflow_strip_height(&wf_parallel);
        assert!(
            h_parallel >= h,
            "Strip height for parallel workflow ({}) should be >= linear ({})",
            h_parallel,
            h
        );
    }

    // ─── Workflow control board dialog rendering ─────────────────────────────────

    fn render_all_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for row in 0..height {
            for col in 0..width {
                text.push_str(buf[(col, row)].symbol());
            }
        }
        text
    }

    #[test]
    fn workflow_control_board_dialog_renders_diamond_labels() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().dialog = crate::tui::state::Dialog::WorkflowControlBoard {
            current_step: "my-step".to_string(),
            error: None,
        };

        let text = render_all_text(&mut app, 80, 30);

        assert!(text.contains("Workflow Control"), "Popup title should appear");
        assert!(text.contains("my-step"), "Step name should appear");
        // Diamond: up arrow at top, down at bottom, left and right in the middle row.
        assert!(text.contains('↑'), "Up arrow (Restart) should appear");
        assert!(text.contains('↓'), "Down arrow (Next: same container) should appear");
        assert!(text.contains('←'), "Left arrow (Cancel to prev) should appear");
        assert!(text.contains('→'), "Right arrow (Next: new container) should appear");
        assert!(text.contains("[Arrow] select"), "Hint line should appear");
    }

    #[test]
    fn workflow_control_board_dialog_renders_error_message() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().dialog = crate::tui::state::Dialog::WorkflowControlBoard {
            current_step: "first-step".to_string(),
            error: Some("No previous step to return to".to_string()),
        };

        let text = render_all_text(&mut app, 80, 30);

        assert!(
            text.contains("No previous step to return to"),
            "Error message should appear in dialog"
        );
    }

    #[test]
    fn workflow_control_board_dialog_no_error_omits_error_line() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().dialog = crate::tui::state::Dialog::WorkflowControlBoard {
            current_step: "some-step".to_string(),
            error: None,
        };

        let text = render_all_text(&mut app, 80, 30);

        assert!(
            !text.contains("No previous step"),
            "Error text should not appear when error is None"
        );
        // But the dialog itself must still render.
        assert!(text.contains("Workflow Control"), "Popup should still render without error");
    }

    #[test]
    fn workflow_control_board_down_arrow_dimmed_with_note_when_next_step_uses_different_agent() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };

        // Two-step workflow where step "a" uses claude and step "b" uses codex.
        let steps = vec![
            crate::workflow::parser::WorkflowStep {
                name: "a".to_string(),
                depends_on: vec![],
                prompt_template: "Step A".to_string(),
                agent: None,
                model: None,
            },
            crate::workflow::parser::WorkflowStep {
                name: "b".to_string(),
                depends_on: vec!["a".to_string()],
                prompt_template: "Step B".to_string(),
                agent: None,
                model: None,
            },
        ];
        let wf = crate::workflow::WorkflowState::new(None, steps, "hash".into(), Some(1), "wf".into());
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("a".to_string());
        app.active_tab_mut().workflow_step_agents.insert("a".to_string(), "claude".to_string());
        app.active_tab_mut().workflow_step_agents.insert("b".to_string(), "codex".to_string());

        app.active_tab_mut().dialog = crate::tui::state::Dialog::WorkflowControlBoard {
            current_step: "a".to_string(),
            error: None,
        };

        let text = render_all_text(&mut app, 80, 30);

        // The ↓ arrow must still appear (dimmed in terminal, but the glyph is present).
        assert!(text.contains('↓'), "Down arrow should still be rendered");
        // The note explaining why same-container is blocked must appear.
        assert!(
            text.contains("codex"),
            "Agent name 'codex' should appear in the note explaining the block. Got: {:?}",
            text
        );
        assert!(
            text.contains("next step uses agent"),
            "Note text should explain that the next step uses a different agent. Got: {:?}",
            text
        );
    }

    #[test]
    fn status_bar_shows_workflow_hint_when_maximized_and_workflow_running() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = crate::tui::state::Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        // Workflow running — hint should mention ctrl-w.
        app.active_tab_mut().workflow = Some(crate::workflow::WorkflowState::new(
            None,
            vec![crate::workflow::parser::WorkflowStep {
                name: "plan".to_string(),
                depends_on: vec![],
                prompt_template: "do it".to_string(),
                agent: None,
                model: None,
            }],
            "hash".to_string(),
            Some(1),
            "wf".to_string(),
        ));
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());

        let text = render_all_text(&mut app, 80, 24);
        assert!(
            text.contains("ctrl-w"),
            "Status bar should mention ctrl-w when maximized with running workflow. Got: {:?}",
            &text[..text.len().min(400)]
        );
    }

    #[test]
    fn status_bar_no_workflow_hint_when_maximized_without_workflow() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "run".into() };
        app.active_tab_mut().focus = crate::tui::state::Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        // No workflow.

        let text = render_all_text(&mut app, 80, 24);
        assert!(
            !text.contains("ctrl-w"),
            "Status bar should not mention ctrl-w when maximized without workflow"
        );
        assert!(text.contains("minimize"), "Minimize hint should still appear");
    }

    // ─── cell_in_selection ───────────────────────────────────────────────────

    #[test]
    fn cell_in_selection_returns_false_when_none() {
        assert!(!cell_in_selection(None, 0, 0));
        assert!(!cell_in_selection(None, 5, 10));
    }

    #[test]
    fn cell_in_selection_single_row_range() {
        // Selection from (2, 3) to (2, 7) — single row, cols 3–7.
        let sel = Some(((2, 3), (2, 7)));
        assert!(cell_in_selection(sel, 2, 3), "start cell should be included");
        assert!(cell_in_selection(sel, 2, 5), "middle cell should be included");
        assert!(cell_in_selection(sel, 2, 7), "end cell should be included");
        assert!(!cell_in_selection(sel, 2, 2), "col before start should be excluded");
        assert!(!cell_in_selection(sel, 2, 8), "col after end should be excluded");
        assert!(!cell_in_selection(sel, 1, 5), "row above should be excluded");
        assert!(!cell_in_selection(sel, 3, 5), "row below should be excluded");
    }

    #[test]
    fn cell_in_selection_multi_row_range() {
        // Selection from (1, 2) to (3, 5).
        let sel = Some(((1, 2), (3, 5)));
        // First row: only cols >= 2
        assert!(cell_in_selection(sel, 1, 2));
        assert!(!cell_in_selection(sel, 1, 1));
        assert!(cell_in_selection(sel, 1, 79), "whole first row after sc is included");
        // Middle row: all cols
        assert!(cell_in_selection(sel, 2, 0));
        assert!(cell_in_selection(sel, 2, 79));
        // Last row: only cols <= 5
        assert!(cell_in_selection(sel, 3, 0));
        assert!(cell_in_selection(sel, 3, 5));
        assert!(!cell_in_selection(sel, 3, 6));
        // Outside row range
        assert!(!cell_in_selection(sel, 0, 5));
        assert!(!cell_in_selection(sel, 4, 0));
    }

    #[test]
    fn cell_in_selection_normalises_reversed_selection() {
        // When start > end, the renderer normalises before calling cell_in_selection.
        // Test that a normalised reversed selection works correctly.
        let (s, e) = ((5u16, 3u16), (2u16, 7u16));
        let norm = if s.0 < e.0 || (s.0 == e.0 && s.1 <= e.1) { (s, e) } else { (e, s) };
        let sel = Some(norm);
        // After normalisation: start=(2,7), end=(5,3)
        assert!(cell_in_selection(sel, 3, 0));
        assert!(cell_in_selection(sel, 2, 7));
        assert!(!cell_in_selection(sel, 5, 4));
        assert!(!cell_in_selection(sel, 1, 0));
    }

    // ─── scrollback indicator rendering ─────────────────────────────────────

    #[test]
    fn scrollback_indicator_appears_when_scrolled() {
        use crate::tui::state::ContainerWindowState;

        let mut app = new_app();
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;

        // Create a parser and feed enough lines to populate scrollback.
        let rows: u16 = 10;
        let cols: u16 = 40;
        app.active_tab_mut().terminal_scrollback_lines = 500;
        app.active_tab_mut().start_container("ctr".into(), "Agent".into(), cols, rows);

        if let Some(ref mut parser) = app.active_tab_mut().vt100_parser {
            for i in 0u32..100 {
                let line = format!("output line {:03}\r\n", i);
                parser.process(line.as_bytes());
            }
        }

        // Set a non-zero scroll offset so the indicator should appear.
        let scroll_offset: usize = 5;
        app.active_tab_mut().container_scroll_offset = scroll_offset;

        // Probe the effective N and max M values using the same logic as the renderer,
        // so we can assert the exact text shown in the indicator.
        let (effective_n, max_m) = {
            let parser = app.active_tab_mut().vt100_parser.as_mut().unwrap();
            parser.set_scrollback(scroll_offset);
            let n = parser.screen().scrollback();
            parser.set_scrollback(usize::MAX);
            let m = parser.screen().scrollback();
            parser.set_scrollback(0);
            (n, m)
        };

        let text = render_all_text(&mut app, 80, 40);

        // The scrollback indicator must contain ↑ and "scrollback".
        assert!(
            text.contains("scrollback"),
            "scrollback indicator should appear when container_scroll_offset > 0; got:\n{}",
            &text[..text.len().min(500)]
        );
        assert!(
            text.contains('↑'),
            "scrollback indicator should contain ↑ arrow"
        );
        // Verify the exact N / M line counts are rendered.
        let expected_counts = format!("{} / {} lines", effective_n, max_m);
        assert!(
            text.contains(&expected_counts),
            "indicator should show '{expected_counts}'; got:\n{}",
            &text[..text.len().min(500)]
        );
        assert!(
            effective_n > 0,
            "effective scroll position must be > 0 when offset={scroll_offset}"
        );
        assert!(
            max_m >= effective_n,
            "max scrollback ({max_m}) must be >= effective position ({effective_n})"
        );
    }

    #[test]
    fn scrollback_indicator_absent_at_live_tail() {
        use crate::tui::state::ContainerWindowState;

        let mut app = new_app();
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        app.active_tab_mut().terminal_scrollback_lines = 500;
        app.active_tab_mut().start_container("ctr".into(), "Agent".into(), 40, 10);

        if let Some(ref mut parser) = app.active_tab_mut().vt100_parser {
            for i in 0u32..20 {
                let line = format!("line {}\r\n", i);
                parser.process(line.as_bytes());
            }
        }

        // scroll_offset = 0 (live tail).
        app.active_tab_mut().container_scroll_offset = 0;

        let text = render_all_text(&mut app, 80, 40);

        assert!(
            !text.contains("scrollback"),
            "scrollback indicator should be absent at live tail"
        );
    }

    // ── popup_width_for ─────────────────────────────────────────────────────

    #[test]
    fn popup_width_for_respects_80_percent_cap() {
        // area_width=100 → max_allowed = 80
        // Items have width 90 (wider than cap) → result must be 80
        let items: Vec<String> = vec!["a".repeat(90)];
        let w = popup_width_for(100, &items, "title");
        assert_eq!(w, 80, "popup width must be capped at 80% of area_width=100");
    }

    #[test]
    fn popup_width_for_fits_content_within_cap() {
        // area_width=200 → max_allowed = 160
        // content is 20 chars + 4 padding = 24, which is < 160
        let items: Vec<String> = vec!["a".repeat(20)];
        let w = popup_width_for(200, &items, "title");
        assert_eq!(w, 24, "popup width should fit content (20 + 4 padding) when under cap");
    }

    #[test]
    fn popup_width_for_uses_title_width_when_wider_than_items() {
        // title = 30 chars, items = 5 chars; content_width = 30 + 4 = 34
        let items: Vec<String> = vec!["hello".to_string()];
        let title = "a".repeat(30);
        let w = popup_width_for(200, &items, &title);
        assert_eq!(w, 34, "popup width should use title width when title is wider than items");
    }

    #[test]
    fn popup_width_for_empty_items_uses_minimum() {
        // No items → max item width = 0 → content_width = max(0, title.len()) + 4
        let items: Vec<String> = vec![];
        let w = popup_width_for(200, &items, "hi");
        // title "hi" = 2, content_width = max(20_default? No — unwrap_or(20).max(title.len())) + 4
        // From impl: items.iter().max() → None → unwrap_or(20).max(title.chars().count()) = 20.max(2) = 20 → 20+4 = 24
        assert_eq!(w, 24, "empty items should fall back to minimum content width of 20 + 4");
    }

    #[test]
    fn popup_width_for_minimum_when_area_very_small() {
        // area_width = 10 → max_allowed = (10*80/100).max(20) = 8.max(20) = 20
        let items: Vec<String> = vec!["hello".to_string()];
        let w = popup_width_for(10, &items, "t");
        // content_width = max(5, 1) + 4 = 9; max_allowed = 20; result = 9
        assert_eq!(w, 9, "tiny area should still produce sensible content-driven width");
    }

    // ── format_session_picker_row ───────────────────────────────────────────

    #[test]
    fn format_session_picker_row_preserves_short_id() {
        let row = format_session_picker_row("abc123", "/home/user/project", 60);
        assert_eq!(row, "abc123  (/home/user/project)");
    }

    #[test]
    fn format_session_picker_row_truncates_long_id() {
        // workdir = "/wd" = 3 chars; max_row_width = 20
        // max_id_chars = 20 - (3 + 6) = 11
        // id = "a" * 20 (longer than 11, and 11 > 3) → truncated to 10 chars + "…"
        let id = "a".repeat(20);
        let row = format_session_picker_row(&id, "/wd", 20);
        // truncated: take(10) = "aaaaaaaaaa" + "…"
        assert!(
            row.starts_with("aaaaaaaaaa…"),
            "long id should be truncated with ellipsis; got: {row}"
        );
        assert!(
            row.contains("(/wd)"),
            "workdir should appear in the row; got: {row}"
        );
    }

    #[test]
    fn format_session_picker_row_no_truncation_when_id_fits() {
        // workdir = 10 chars, max_row_width = 40 → max_id_chars = 40 - 16 = 24
        // id = 10 chars → no truncation
        let row = format_session_picker_row("short-id", "/work/dir1", 40);
        assert_eq!(row, "short-id  (/work/dir1)");
    }

    #[test]
    fn format_session_picker_row_no_truncation_when_max_id_chars_too_small() {
        // max_id_chars <= 3 → no truncation even if id is long
        // workdir = 30 chars, max_row_width = 35 → max_id_chars = 35 - 36 = saturating = 0
        let workdir = "a".repeat(30);
        let id = "long-id-123456789";
        let row = format_session_picker_row(id, &workdir, 35);
        // max_id_chars = 0, which is not > 3, so no truncation
        assert!(
            row.contains(id),
            "id should not be truncated when max_id_chars <= 3; got: {row}"
        );
    }

    // ─── Remote-bound tab and new-tab dialog render tests (work item 0061) ────

    /// Serialise env-var mutations: env is process-global state, so tests that
    /// mutate `AMUX_REMOTE_ADDR` must hold this lock for the duration.
    static REMOTE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Collect the full text content of a rendered frame into a single `String`.
    fn full_buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut text = String::new();
        for row in area.y..(area.y + area.height) {
            for col in area.x..(area.x + area.width) {
                text.push_str(buf[(col, row)].symbol());
            }
        }
        text
    }

    /// New-tab dialog renders the remote session list when `remote_sessions =
    /// Some(Ok([...]))`.  The display should include a short ID prefix and the
    /// workdir, plus the "Create new remote session" sentinel at the end.
    #[test]
    fn new_tab_dialog_renders_session_list_when_sessions_available() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::NewTabDirectory {
            input: String::new(),
            remote_sessions: Some(Ok(vec![
                crate::commands::remote::RemoteSessionEntry {
                    id: "abc12345-xxxx".to_string(),
                    workdir: "/workspace/myproject".to_string(),
                },
            ])),
            remote_selected_idx: Some(0),
            focus_workdir: false,
        };

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let text = full_buffer_text(terminal.backend().buffer());

        // The first 8 chars of the session ID must appear.
        assert!(
            text.contains("abc12345"),
            "dialog must render the session ID (first 8 chars); buffer:\n{text}"
        );
        // The workdir must appear.
        assert!(
            text.contains("/workspace/myproject"),
            "dialog must render the session workdir; buffer:\n{text}"
        );
    }

    /// New-tab dialog renders the "Create new remote session" sentinel as the
    /// last item after the session list.
    #[test]
    fn new_tab_dialog_renders_create_new_session_button() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::NewTabDirectory {
            input: String::new(),
            remote_sessions: Some(Ok(vec![
                crate::commands::remote::RemoteSessionEntry {
                    id: "sess-0001-aaaa".to_string(),
                    workdir: "/tmp/p".to_string(),
                },
            ])),
            remote_selected_idx: Some(0),
            focus_workdir: false,
        };

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let text = full_buffer_text(terminal.backend().buffer());

        assert!(
            text.contains("Create new remote session"),
            "dialog must render '+ Create new remote session' as the last list item; \
             buffer:\n{text}"
        );
    }

    /// New-tab dialog renders the ⚠ warning when `remote_sessions = Some(Err(...))`.
    #[test]
    fn new_tab_dialog_renders_error_when_sessions_fetch_failed() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::NewTabDirectory {
            input: String::new(),
            remote_sessions: Some(Err("connection refused".to_string())),
            remote_selected_idx: None,
            focus_workdir: true,
        };

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let text = full_buffer_text(terminal.backend().buffer());

        assert!(
            text.contains("Could not reach remote"),
            "dialog must render 'Could not reach remote' warning on error; buffer:\n{text}"
        );
        assert!(
            text.contains("connection refused"),
            "dialog must render the specific error message; buffer:\n{text}"
        );
    }

    /// The tab bar renders the remote session's `display_host` as the tab title
    /// for a remote-bound tab (instead of the local working directory name).
    /// The display_host is kept short (≤14 chars) so it is not truncated.
    #[test]
    fn tab_bar_renders_display_host_as_title_for_remote_bound_tab() {
        let mut app = new_app();
        // "10.0.1.5:9000" is 13 chars — under the 14-char truncation limit.
        app.active_tab_mut().remote_binding = Some(crate::tui::state::RemoteTabBinding {
            remote_addr: "http://10.0.1.5:9000".to_string(),
            session_id: "remote-sess".to_string(),
            api_key: None,
            display_host: "10.0.1.5:9000".to_string(),
        });

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let text = full_buffer_text(terminal.backend().buffer());

        assert!(
            text.contains("10.0.1.5:9000"),
            "tab bar must display the remote host:port for a remote-bound tab; buffer:\n{text}"
        );
    }

    /// The tab bar uses `Color::Magenta` for the border of a remote-bound tab.
    ///
    /// We verify this by inspecting the foreground color of the tab border cells
    /// in the rendered buffer (top-left corner of the first tab, row 0).
    #[test]
    fn tab_bar_border_is_magenta_for_remote_bound_tab() {
        let mut app = new_app();
        app.active_tab_mut().remote_binding = Some(crate::tui::state::RemoteTabBinding {
            remote_addr: "http://10.0.0.1:9000".to_string(),
            session_id: "rsess".to_string(),
            api_key: None,
            display_host: "10.0.0.1:9000".to_string(),
        });

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();

        // The first tab occupies columns 0-19, rows 0-2.
        // The top-left border character is at (0, 0).
        let border_fg = buf[(0u16, 0u16)].style().fg;
        assert_eq!(
            border_fg,
            Some(ratatui::style::Color::Magenta),
            "tab bar border must be Magenta for a remote-bound tab; got fg: {:?}",
            border_fg
        );
    }

    /// New-tab dialog renders "Loading remote sessions…" when `remote_sessions = None`
    /// and `remote.defaultAddr` is configured.  The loading placeholder must appear
    /// while the async session fetch is in-flight.
    #[test]
    fn new_tab_dialog_renders_loading_when_remote_sessions_is_none() {
        let _guard = REMOTE_ENV_LOCK.lock().unwrap();

        // Write a temporary global config with remote.defaultAddr set so
        // `effective_remote_default_addr()` returns Some(...) during the render.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = serde_json::json!({"remote": {"defaultAddr": "http://10.0.0.1:9876"}});
        std::fs::write(tmp.path().join("config.json"), cfg.to_string()).unwrap();
        // SAFETY: test-only; serialised by REMOTE_ENV_LOCK.
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap()) };

        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::NewTabDirectory {
            input: String::new(),
            remote_sessions: None,
            remote_selected_idx: None,
            focus_workdir: true,
        };

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };

        let text = full_buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("Loading remote sessions"),
            "dialog must render loading placeholder when remote_sessions is None \
             and remote.defaultAddr is configured; buffer:\n{text}"
        );
    }

    /// Workflow state strip renders correctly for a remote-sourced `WorkflowState`
    /// on a remote-bound tab.  Remote tabs populate `tab.workflow` via the polling
    /// channel; the renderer must use the same strip path as local workflows.
    #[test]
    fn workflow_strip_renders_for_remote_bound_tab() {
        use crate::workflow::{WorkflowState, WorkflowStepState, StepStatus};

        let mut app = new_app();

        // Attach a remote binding.
        app.active_tab_mut().remote_binding = Some(crate::tui::state::RemoteTabBinding {
            remote_addr: "http://10.0.0.2:9876".to_string(),
            session_id: "remote-sess-wf".to_string(),
            api_key: None,
            display_host: "10.0.0.2:9876".to_string(),
        });

        // Build a running workflow state (same structure as local workflows).
        let steps = vec![
            WorkflowStepState {
                name: "plan".to_string(),
                depends_on: vec![],
                prompt_template: "Plan the work.".to_string(),
                status: StepStatus::Done,
                container_id: None,
                agent: None,
                model: None,
            },
            WorkflowStepState {
                name: "implement".to_string(),
                depends_on: vec!["plan".to_string()],
                prompt_template: "Implement it.".to_string(),
                status: StepStatus::Running,
                container_id: None,
                agent: None,
                model: None,
            },
        ];
        let wf = WorkflowState {
            title: Some("Remote Workflow".to_string()),
            steps,
            workflow_hash: "abc123".to_string(),
            work_item: Some(61),
            workflow_name: "remote-wf".to_string(),
        };
        app.active_tab_mut().workflow = Some(wf);
        // Remote tabs don't set workflow_current_step (they use the polled state directly).
        // The strip renders based on WorkflowState alone.

        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let text = full_buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("plan") || text.contains("impl"),
            "workflow strip must render step names for a remote-bound tab; buffer:\n{}",
            &text[..text.len().min(400)]
        );
    }
}
