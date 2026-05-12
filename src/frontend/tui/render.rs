//! UI chrome rendering — frame layout, tab bar, execution window, status bar,
//! command box, suggestion row.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::container_view;
use crate::frontend::tui::dialogs;
use crate::frontend::tui::tabs::{
    self, compute_tab_bar_width, phase_label, tab_color, window_border_color, ContainerWindowState,
    ExecutionPhase,
};
use crate::frontend::tui::workflow_view;

/// Render the full TUI frame.
pub fn render_frame(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Read shape decisions from the active tab (immutable borrow).
    let (workflow_height, container_state, has_summary) = {
        let tab = app.active_tab();
        let workflow_height = tab
            .workflow_state
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(workflow_view::workflow_strip_height))
            .unwrap_or(0);
        (
            workflow_height,
            tab.container_window_state,
            tab.last_container_summary.is_some(),
        )
    };

    let has_minimized_container = container_state == ContainerWindowState::Minimized;
    // Show the post-exit summary in the same slot as the minimized bar, but
    // only when the container is Hidden (i.e. the previous run finished and
    // we haven't started another).
    let has_summary_bar =
        !has_minimized_container && container_state == ContainerWindowState::Hidden && has_summary;

    let extra_bar_height = if has_minimized_container || has_summary_bar {
        3
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Length(3),                // tab bar
        Constraint::Min(5),                   // execution window
        Constraint::Length(extra_bar_height), // minimized OR summary
        Constraint::Length(workflow_height),  // workflow strip
        Constraint::Length(1),                // status bar
        Constraint::Length(3),                // command box
        Constraint::Length(1),                // suggestion row
    ])
    .split(area);

    render_tab_bar(app, chunks[0], frame);
    render_execution_window(app, chunks[1], frame);

    if has_minimized_container {
        container_view::render_container_minimized(app.active_tab(), chunks[2], frame);
    } else if has_summary_bar {
        if let Some(summary) = app.active_tab().last_container_summary.as_ref() {
            container_view::render_container_summary(summary, chunks[2], frame);
        }
    }

    if let Some(wf_state) = app
        .active_tab()
        .workflow_state
        .lock()
        .ok()
        .and_then(|g| g.clone())
    {
        let scroll_offset = app.active_tab().workflow_strip_scroll_offset;
        workflow_view::render_workflow_strip(&wf_state, chunks[3], frame, scroll_offset);
        app.active_tab_mut().last_strip_rect = Some(chunks[3]);
    }

    render_status_bar(app, chunks[4], frame);
    render_command_box(app, chunks[5], frame);
    render_suggestion_row(app, chunks[6], frame);

    // Container maximized overlay (rendered on top of execution window only,
    // not over the workflow strip or bottom chrome).
    if container_state == ContainerWindowState::Maximized {
        container_view::render_container_maximized(
            app.active_tab_mut(),
            area,
            workflow_height,
            frame,
        );
    }

    // Active dialog (rendered on top of everything).
    if let Some(dialog) = &app.active_dialog {
        render_dialog(dialog, area, frame);
    }
}

/// Render the tab bar — matches old amux:
/// - 3-row tall cells with rounded borders
/// - Active tab: omits the bottom border so it visually merges into the
///   execution window below; title gets `➡` prefix and is bold + tab color
/// - Inactive tab: full borders; title is DarkGray (subdued)
/// - Subcommand label rendered INSIDE the cell as content (1 row),
///   not in the title
/// - Width is derived from each tab's natural content width, capped against
///   the budget (¼/½/¾/1/n for n=1/2/3/n tabs)
fn render_tab_bar(app: &App, area: Rect, frame: &mut Frame) {
    let n = app.tabs.len();
    if n == 0 || area.width == 0 {
        return;
    }

    // First pass: compute the maximum natural content width across all tabs.
    // We pass `u16::MAX` as the cell width to `tab_subcommand_label` so it
    // doesn't truncate while measuring.
    let max_natural_content: u16 = app
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let is_active = i == app.active_tab;
            let project = tab.project_name();
            // Title interior: `" ➡ {project} "` = project + 4 chars
            // (or `" {project} "` = project + 2 chars when not active);
            // we always size for the wider variant so the active toggle
            // doesn't reflow the bar.
            let title_inner = (project.chars().count() as u16).saturating_add(4);
            let subcmd = tab.tab_subcommand_label(u16::MAX, is_active);
            // Body interior: `" {subcmd} "` = subcmd + 2 chars
            let content_inner = (subcmd.chars().count() as u16).saturating_add(2);
            title_inner.max(content_inner)
        })
        .max()
        .unwrap_or(18);

    let tab_width = compute_tab_bar_width(n, area.width, max_natural_content);
    if tab_width == 0 {
        return;
    }

    for (i, tab) in app.tabs.iter().enumerate() {
        let x = area.x + (i as u16) * tab_width;
        // Stop drawing when the next cell would overflow — old amux did the
        // same; there is no overflow indicator.
        if x + tab_width > area.x + area.width {
            break;
        }
        let is_active = i == app.active_tab;
        let tab_area = Rect::new(x, area.y, tab_width, 3);
        let color = tab_color(tab);
        let project = tab.project_name();
        let subcmd = tab.tab_subcommand_label(tab_width, is_active);

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
            format!(" \u{27a1} {} ", project)
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

/// Render the execution window — rounded border with the phase label as the
/// left-aligned title; border color from `window_border_color(phase, focused)`.
///
/// Body content:
/// - Idle (and the status log is empty): a 3-line welcome stub in DarkGray.
/// - Otherwise: the status log entries, colored per level, with `Wrap{trim:false}`.
fn render_execution_window(app: &App, area: Rect, frame: &mut Frame) {
    let tab = app.active_tab();
    let focused = app.focus == Focus::ExecutionWindow;
    let border_color = window_border_color(&tab.execution_phase, focused);
    let title = phase_label(&tab.execution_phase);

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Status dashboard takes priority when populated by the status command.
    let has_dashboard = tab
        .status_dashboard
        .lock()
        .map(|d| d.is_some())
        .unwrap_or(false);
    if has_dashboard {
        render_status_dashboard(tab, inner, frame);
        return;
    }

    let log_empty = tab
        .status_log
        .lock()
        .map(|log| log.is_empty())
        .unwrap_or(true);

    if matches!(tab.execution_phase, ExecutionPhase::Idle) && log_empty {
        // Three-line welcome stub matching old amux exactly.
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Welcome to amux.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  Running `amux ready` to check your environment...",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        render_output_content(tab, inner, frame);
    }
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
    let log = match tab.status_log.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if log.is_empty() {
        return;
    }

    if tab.status_log_collapsed {
        let last = &log[log.len() - 1];
        let color = status_level_color(&last.level);
        let line = Line::from(Span::styled(&last.text, Style::default().fg(color)));
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }

    let lines: Vec<Line> = log
        .iter()
        .map(|entry| {
            let color = status_level_color(&entry.level);
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

/// Render the status dashboard as a proper ratatui `Table` widget.
fn render_status_dashboard(tab: &tabs::Tab, area: Rect, frame: &mut Frame) {
    let dash = match tab.status_dashboard.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let data = match dash.as_ref() {
        Some(d) => d,
        None => return,
    };

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // Title + empty line above the table.
    let title_height: u16 = 2;
    let tip_height: u16 = 2;
    let table_area_height = area.height.saturating_sub(title_height + tip_height);

    // Split: title row, table, tip row.
    let chunks = Layout::vertical([
        Constraint::Length(title_height),
        Constraint::Length(table_area_height),
        Constraint::Length(tip_height),
    ])
    .split(area);

    // Title.
    let title = Paragraph::new(vec![
        Line::from(Span::styled(
            " AMUX STATUS DASHBOARD",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ]);
    frame.render_widget(title, chunks[0]);

    if data.containers.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled(
                " No code agents running.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                " To start one:  amux exec workflow <file>  or  amux chat",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(empty, chunks[1]);
    } else {
        let header = Row::new(vec![
            Cell::from(" "),
            Cell::from("NAME").style(header_style),
            Cell::from("CPU").style(header_style),
            Cell::from("MEM").style(header_style),
            Cell::from("IMAGE").style(header_style),
            Cell::from("TAB").style(header_style),
        ])
        .bottom_margin(0);

        let rows: Vec<Row> = data
            .containers
            .iter()
            .map(|c| {
                let indicator_style = if c.stuck {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                };
                let indicator = "\u{25cf}";

                let cpu = c
                    .cpu_percent
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "-".into());
                let mem = c
                    .memory_mb
                    .map(|v| format!("{v:.0}MB"))
                    .unwrap_or_else(|| "-".into());
                let tab_label = c
                    .tab_number
                    .map(|t| format!("{t}"))
                    .unwrap_or_else(|| "-".into());

                Row::new(vec![
                    Cell::from(indicator).style(indicator_style),
                    Cell::from(c.name.as_str()),
                    Cell::from(cpu),
                    Cell::from(mem),
                    Cell::from(c.image.as_str()),
                    Cell::from(tab_label),
                ])
            })
            .collect();

        // Column widths: start from header label widths, then expand to the
        // widest value in each column, then add 2 chars of trailing padding.
        let pad: usize = 2;
        let mut w_indicator: usize = 1;
        let mut w_name: usize = "NAME".len();
        let mut w_cpu: usize = "CPU".len();
        let mut w_mem: usize = "MEM".len();
        let mut w_image: usize = "IMAGE".len();
        let mut w_tab: usize = "TAB".len();
        for c in &data.containers {
            w_indicator = w_indicator.max(1);
            w_name = w_name.max(c.name.len());
            let cpu = c
                .cpu_percent
                .map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| "-".into());
            w_cpu = w_cpu.max(cpu.len());
            let mem = c
                .memory_mb
                .map(|v| format!("{v:.0}MB"))
                .unwrap_or_else(|| "-".into());
            w_mem = w_mem.max(mem.len());
            w_image = w_image.max(c.image.len());
            let tab_label = c
                .tab_number
                .map(|t| format!("{t}"))
                .unwrap_or_else(|| "-".into());
            w_tab = w_tab.max(tab_label.len());
        }
        let widths = [
            Constraint::Length((w_indicator + pad) as u16),
            Constraint::Length((w_name + pad) as u16),
            Constraint::Length((w_cpu + pad) as u16),
            Constraint::Length((w_mem + pad) as u16),
            Constraint::Length((w_image + pad) as u16),
            Constraint::Length((w_tab + pad) as u16),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(Style::default());
        frame.render_widget(table, chunks[1]);
    }

    // Tip.
    let tip = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(" Tip: {}", data.tip),
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(tip, chunks[2]);
}

/// Render the 1-row status hint bar above the command box.
///
/// Content is a `(phase, focus, container)` decision matrix copied from
/// old amux: tells the user which keybinding is most relevant right now
/// (Esc to deselect, ↑ to focus the window, ctrl-m to cycle the container,
/// etc.). Background is forced black so the row stands out against the
/// surrounding chrome.
fn render_status_bar(app: &App, area: Rect, frame: &mut Frame) {
    use crate::frontend::tui::tabs::{ContainerWindowState, ExecutionPhase};

    let tab = app.active_tab();
    let workflow_active = tab
        .workflow_state
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);

    let spans: Vec<Span> = match (&tab.execution_phase, app.focus, tab.container_window_state) {
        // Running + ExecWindow + Maximized container
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Maximized,
        ) => {
            if workflow_active {
                vec![Span::styled(
                    " ctrl-m minimize  \u{00b7}  ctrl-w workflow controls ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " ctrl-m minimize  \u{00b7}  scroll \u{2195} history ",
                    Style::default().fg(Color::Yellow),
                )]
            }
        }
        // Running + ExecWindow + Minimized container
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Minimized,
        ) => {
            vec![Span::styled(
                " \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  ctrl-m restore container  \u{00b7}  Esc deselect ",
                Style::default().fg(Color::DarkGray),
            )]
        }
        // Running + ExecWindow + no container
        (ExecutionPhase::Running { .. }, Focus::ExecutionWindow, ContainerWindowState::Hidden) => {
            vec![Span::styled(
                " Press Esc to deselect the window ",
                Style::default().fg(Color::Yellow),
            )]
        }
        // Running + CommandBox
        (ExecutionPhase::Running { .. }, Focus::CommandBox, _) => {
            if workflow_active {
                vec![Span::styled(
                    " Press ctrl-w for workflow controls ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " Press \u{2191} to focus the window ",
                    Style::default().fg(Color::DarkGray),
                )]
            }
        }
        // Done + ExecWindow
        (ExecutionPhase::Done { .. }, Focus::ExecutionWindow, _) => vec![Span::styled(
            " \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  Esc deselect ",
            Style::default().fg(Color::DarkGray),
        )],
        // Done + CommandBox
        (ExecutionPhase::Done { .. }, Focus::CommandBox, _) => vec![Span::styled(
            " Press \u{2191} to focus the window ",
            Style::default().fg(Color::DarkGray),
        )],
        // Error + ExecWindow
        (ExecutionPhase::Error { .. }, Focus::ExecutionWindow, _) => {
            let exit_code = match &tab.execution_phase {
                ExecutionPhase::Error { .. } => -1,
                ExecutionPhase::Done { exit_code, .. } => *exit_code,
                _ => 0,
            };
            vec![
                Span::styled(
                    format!(" Exit code: {} ", exit_code),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " \u{00b7}  \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  Esc deselect ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        // Error + CommandBox
        (ExecutionPhase::Error { .. }, Focus::CommandBox, _) => {
            let exit_code = match &tab.execution_phase {
                ExecutionPhase::Error { .. } => -1,
                ExecutionPhase::Done { exit_code, .. } => *exit_code,
                _ => 0,
            };
            vec![
                Span::styled(
                    format!(" Exit code: {} ", exit_code),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " \u{00b7}  Press \u{2191} to focus the window ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        // Idle: empty row (just the black background).
        _ => vec![],
    };

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}

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
fn render_command_box(app: &App, area: Rect, frame: &mut Frame) {
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
    let scroll_offset = if cursor_col >= visible_width {
        cursor_col - visible_width + 1
    } else {
        0
    };
    let visible_text: String = display_text.chars().skip(scroll_offset).collect();
    let line = Line::from(vec![prefix, Span::raw(visible_text)]);
    frame.render_widget(Paragraph::new(line), inner);

    if focused && app.active_dialog.is_none() {
        let display_cursor_x = (cursor_col - scroll_offset) as u16;
        let cursor_x = area.x + 1 + 2 + display_cursor_x;
        let cursor_y = area.y + 1;
        if cursor_x < area.x + area.width.saturating_sub(1) {
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
    }
}

/// Render the 1-row suggestion / context line below the command box.
///
/// Dual purpose:
/// - When the command box is focused AND there are autocomplete suggestions:
///   render them separated by `"  ·  "` in DarkGray with each suggestion in
///   Cyan.
/// - Otherwise: fall back to a `"  CWD: {path}"` line (or `"  Using
///   Worktree: {path}"` when the active tab is bound to a worktree).
fn render_suggestion_row(app: &App, area: Rect, frame: &mut Frame) {
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
fn truncate_middle(s: &str, max: usize) -> String {
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
fn status_level_color(level: &crate::engine::message::MessageLevel) -> Color {
    use crate::engine::message::MessageLevel;
    match level {
        MessageLevel::Info => Color::White,
        MessageLevel::Warning => Color::Yellow,
        MessageLevel::Error => Color::Red,
        MessageLevel::Success => Color::Green,
    }
}

/// Render the currently active dialog.
fn render_dialog(dialog: &dialogs::Dialog, area: Rect, frame: &mut Frame) {
    match dialog {
        dialogs::Dialog::QuitConfirm => {
            dialogs::render_quit_confirm(area, frame);
        }
        dialogs::Dialog::CloseTabConfirm => {
            dialogs::render_close_tab_confirm(area, frame);
        }
        dialogs::Dialog::WorkflowCancelConfirm => {
            dialogs::render_workflow_cancel_confirm(area, frame);
        }
        dialogs::Dialog::YesNo { title, body } => {
            dialogs::render_yes_no(title, body, area, frame);
        }
        dialogs::Dialog::YesNoCancel { title, body } => {
            // Same dynamic sizing as render_yes_no, plus an explicit Cancel.
            let max_w = area.width.saturating_sub(6).max(40);
            let max_body_w = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = max_body_w.saturating_add(6).max(50).max(title_w).min(max_w);
            let inner_w = width.saturating_sub(4) as usize;
            let wrapped_lines: usize = body
                .lines()
                .map(|line| {
                    let w = unicode_width::UnicodeWidthStr::width(line);
                    if inner_w == 0 || w == 0 {
                        1
                    } else {
                        w.div_ceil(inner_w)
                    }
                })
                .sum();
            let body_h = wrapped_lines as u16;
            let height = (body_h + 5).min(area.height.saturating_sub(2)).max(7);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let text = format!("{body}\n\n  [y] Yes   [n] No   [Esc] Cancel");
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::TextInput {
            title,
            prompt,
            editor,
        } => {
            // Layout: prompt (multi-line) + spacer + bordered input + spacer +
            // hint row. Width grows with terminal but caps at 80.
            let prompt_lines = prompt.lines().count() as u16;
            let dialog_h = prompt_lines + 9;
            let dialog_w = (area.width.saturating_sub(8)).clamp(50, 80);
            let dialog_area = dialogs::centered_fixed(dialog_w, dialog_h, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            let prompt_area = Rect {
                height: prompt_lines,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(prompt.as_str()).style(Style::default().fg(Color::Gray)),
                prompt_area,
            );
            let input_area = Rect {
                y: inner.y + prompt_lines + 1,
                height: 3,
                ..inner
            };
            let input_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            let input_inner = input_block.inner(input_area);
            frame.render_widget(input_block, input_area);
            let display_text: String = editor
                .text
                .chars()
                .take(input_inner.width as usize)
                .collect();
            frame.render_widget(
                Paragraph::new(display_text).style(Style::default().fg(Color::White)),
                input_inner,
            );
            // Hint row below the input.
            let hint_y = input_area.y + input_area.height + 1;
            if hint_y < inner.y + inner.height {
                let hint_area = Rect {
                    y: hint_y,
                    height: 1,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new("  [Enter] submit   [Esc] cancel")
                        .style(Style::default().fg(Color::DarkGray)),
                    hint_area,
                );
            }
            let text_before_cursor = &editor.text[..editor.cursor];
            let cursor_display_w = unicode_width::UnicodeWidthStr::width(text_before_cursor) as u16;
            let cursor_x =
                input_inner.x + cursor_display_w.min(input_inner.width.saturating_sub(1));
            let cursor_y = input_inner.y;
            if cursor_x < input_inner.x + input_inner.width {
                frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            }
        }
        dialogs::Dialog::MultilineInput {
            title,
            prompt,
            editor,
        } => {
            let dialog_area = dialogs::centered_rect(70, 60, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);

            // Layout: prompt lines, 1-row gap, bordered textarea, 1-row gap, hint.
            let prompt_lines = prompt.lines().count() as u16;
            let prompt_area = Rect {
                height: prompt_lines,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(prompt.as_str()).style(Style::default().fg(Color::Gray)),
                prompt_area,
            );

            // Textarea with a visible border.
            let textarea_y = inner.y + prompt_lines + 1;
            let hint_reserve: u16 = 2; // 1-row gap + 1-row hint
            let textarea_h = inner
                .height
                .saturating_sub(prompt_lines + 1 + hint_reserve)
                .max(3);
            let textarea_area = Rect {
                x: inner.x,
                y: textarea_y,
                width: inner.width,
                height: textarea_h,
            };
            let textarea_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            let textarea_inner = textarea_block.inner(textarea_area);
            frame.render_widget(textarea_block, textarea_area);

            // Render editor text inside the bordered textarea with wrapping.
            let inner_w = textarea_inner.width as usize;
            let inner_h = textarea_inner.height as usize;

            // Compute visual lines from the editor text (split by '\n', then
            // wrap each logical line at inner_w).
            let logical_lines: Vec<&str> = editor.text.split('\n').collect();
            let mut visual_lines: Vec<String> = Vec::new();
            for line in &logical_lines {
                if line.is_empty() {
                    visual_lines.push(String::new());
                } else if inner_w == 0 {
                    visual_lines.push(line.to_string());
                } else {
                    let chars: Vec<char> = line.chars().collect();
                    for chunk in chars.chunks(inner_w) {
                        visual_lines.push(chunk.iter().collect());
                    }
                }
            }

            // Compute cursor position in visual-line space.
            let text_before_cursor = &editor.text[..editor.cursor];
            let cursor_logical: Vec<&str> = text_before_cursor.split('\n').collect();
            let cursor_last_line = cursor_logical.last().unwrap_or(&"");
            let cursor_col_chars = cursor_last_line.chars().count();
            let mut cursor_visual_row: usize = 0;
            // Walk logical lines before the cursor line.
            for (i, line) in logical_lines.iter().enumerate() {
                if i >= cursor_logical.len() - 1 {
                    break;
                }
                let line_chars = line.chars().count();
                if line_chars == 0 || inner_w == 0 {
                    cursor_visual_row += 1;
                } else {
                    cursor_visual_row += line_chars.div_ceil(inner_w);
                }
            }
            // Add wrapped rows from the current logical line.
            if inner_w > 0 && cursor_col_chars > 0 {
                cursor_visual_row += cursor_col_chars / inner_w;
            }
            let cursor_visual_col = if inner_w > 0 {
                cursor_col_chars % inner_w
            } else {
                cursor_col_chars
            };

            // Scroll to keep cursor visible.
            let scroll_offset = if cursor_visual_row >= inner_h {
                cursor_visual_row - inner_h + 1
            } else {
                0
            };

            // Render visible lines.
            let visible: Vec<Line> = visual_lines
                .iter()
                .skip(scroll_offset)
                .take(inner_h)
                .map(|s| Line::from(s.as_str()))
                .collect();
            frame.render_widget(
                Paragraph::new(visible).style(Style::default().fg(Color::White)),
                textarea_inner,
            );

            // Hint row below the textarea.
            let hint_y = textarea_area.y + textarea_area.height + 1;
            if hint_y < inner.y + inner.height {
                let hint_area = Rect {
                    y: hint_y,
                    height: 1,
                    ..inner
                };
                frame.render_widget(
                    Paragraph::new(
                        "  [Ctrl+Enter / Ctrl+S] submit   [Enter] newline   [Esc] cancel",
                    )
                    .style(Style::default().fg(Color::DarkGray)),
                    hint_area,
                );
            }

            // Place the cursor at the correct visual position.
            let display_row = cursor_visual_row.saturating_sub(scroll_offset);
            let cx = textarea_inner.x
                + (cursor_visual_col as u16).min(textarea_inner.width.saturating_sub(1));
            let cy = textarea_inner.y + display_row as u16;
            if cx < textarea_inner.x + textarea_inner.width
                && cy < textarea_inner.y + textarea_inner.height
            {
                frame.set_cursor_position(Position::new(cx, cy));
            }
        }
        dialogs::Dialog::ListPicker {
            title,
            items,
            selected,
        } => {
            // Width fits the longest item plus margin/prefix; height fits up
            // to all items plus a hint, capped to the terminal area.
            let max_item_w = items
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = (max_item_w + 8)
                .max(title_w)
                .max(50)
                .min(area.width.saturating_sub(4));
            let body_h = items.len() as u16 + 1; // +1 for the hint row
            let height = (body_h + 4).min(area.height.saturating_sub(2)).max(7);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            // Reserve last row for the hint.
            let list_h = inner.height.saturating_sub(1);
            let list_area = Rect {
                height: list_h,
                ..inner
            };
            // Window items so the selection stays visible when the list is
            // taller than the dialog.
            let visible = list_h as usize;
            let start = selected
                .saturating_sub(visible.saturating_sub(1))
                .min(items.len().saturating_sub(visible).max(0));
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(i, item)| {
                    let prefix = if i == *selected { "▸ " } else { "  " };
                    let style = if i == *selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    Line::from(Span::styled(format!("{prefix}{item}"), style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), list_area);
            let hint_area = Rect {
                y: inner.y + list_h,
                height: 1,
                ..inner
            };
            frame.render_widget(
                Paragraph::new("  [↑/↓] navigate   [Enter] select   [Esc] cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                hint_area,
            );
        }
        dialogs::Dialog::KindSelect { title, options } => {
            let max_label_w = options
                .iter()
                .map(|(_k, l)| unicode_width::UnicodeWidthStr::width(l.as_str()))
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = (max_label_w + 12)
                .max(title_w)
                .max(50)
                .min(area.width.saturating_sub(4));
            let body_h = options.len() as u16 + 1; // +1 for hint
            let height = (body_h + 4).min(area.height.saturating_sub(2)).max(7);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines: Vec<Line> = options
                .iter()
                .enumerate()
                .map(|(i, (_key, label))| Line::from(format!("  [{}] {label}", i + 1)))
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [1-9] select   [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::WorkflowControlBoard(state) => {
            let extra_reasons = [
                state.continue_unavailable_reason.is_some(),
                state.cancel_to_previous_unavailable_reason.is_some(),
                state.finish_workflow_unavailable_reason.is_some(),
            ]
            .iter()
            .filter(|x| **x)
            .count() as u16;
            let base_height: u16 = if state.can_finish { 14 } else { 12 };
            // Width fits the longest reason line (+ left margin) when present;
            // otherwise the diamond layout's natural minimum is comfortable.
            let max_reason_w = [
                state.continue_unavailable_reason.as_deref(),
                state.cancel_to_previous_unavailable_reason.as_deref(),
                state.finish_workflow_unavailable_reason.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(|s| unicode_width::UnicodeWidthStr::width(s) + 15)
            .max()
            .unwrap_or(0) as u16;
            let step_w =
                unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16 + 10;
            let width = max_reason_w
                .max(step_w)
                .max(56)
                .min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, base_height + extra_reasons, area);
            let title = if state.can_dismiss {
                "Workflow Control (step running)"
            } else {
                "Workflow Control"
            };
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);

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

            let mut lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::raw(" Step: "),
                    Span::styled(&state.step_name, step_style),
                ]),
                Line::from(""),
                // ↑ Restart (top of diamond)
                Line::from(vec![
                    Span::raw("         "),
                    Span::styled("\u{2191}", arrow_style),
                    Span::styled(" Restart current step", label_style),
                ]),
                Line::from(""),
                // ← Cancel to prev    → Next: new container
                Line::from(vec![
                    Span::styled("\u{2190}", left_arrow_style),
                    Span::styled(" Cancel to prev", left_label_style),
                    Span::raw("   "),
                    Span::styled("\u{2192}", right_arrow_style),
                    Span::styled(" Next: new container", right_label_style),
                ]),
                Line::from(""),
                // ↓ Next: same container (bottom of diamond)
                Line::from(vec![
                    Span::raw("         "),
                    Span::styled("\u{2193}", down_arrow_style),
                    Span::styled(" Next: same container", down_label_style),
                ]),
            ];
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
            } else {
                lines.push(Line::from(Span::styled(
                    "  [^C] Abort   [Esc] Pause",
                    dimmed_style,
                )));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::WorkflowStepError(state) => {
            let max_err_w = state
                .error_lines
                .iter()
                .map(|l| unicode_width::UnicodeWidthStr::width(l.as_str()))
                .max()
                .unwrap_or(0) as u16;
            let step_w =
                unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16 + 10; // "  Step: " prefix.
            let width = max_err_w
                .max(step_w)
                .saturating_add(6)
                .max(60)
                .min(area.width.saturating_sub(4));
            let height = (state.error_lines.len() as u16 + 8)
                .min(area.height.saturating_sub(4))
                .max(9);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame("Step failed", Color::Red, dialog_area, frame);
            let mut lines = vec![
                Line::from(format!("  Step: {}", state.step_name)),
                Line::from(""),
            ];
            for line in &state.error_lines {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [r] Retry   [q/Esc] Pause   [a] Abort",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::WorkflowYoloCountdown(state) => {
            let emoji = if state.remaining_secs % 2 == 0 {
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
        dialogs::Dialog::AgentSetup(state) => {
            let title = if state.image_only {
                format!("Build {} image?", state.agent_name)
            } else {
                format!("Set up {}?", state.agent_name)
            };
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let fallback_w = state
                .fallback_name
                .as_deref()
                .map(unicode_width::UnicodeWidthStr::width)
                .unwrap_or(0) as u16
                + 22;
            let width = title_w
                .max(fallback_w)
                .max(55)
                .min(area.width.saturating_sub(4));
            let height = if state.has_fallback && state.fallback_name.is_some() {
                10
            } else {
                9
            };
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(&title, Color::Yellow, dialog_area, frame);
            let mut lines = vec![Line::from(""), Line::from("  [y] Yes   [n] No")];
            if state.has_fallback {
                if let Some(ref fb) = state.fallback_name {
                    lines.push(Line::from(format!("  [f] Fallback to {fb}")));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc] Abort",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::MountScope(state) => {
            // Paths can be long — auto-grow to fit, but cap to area.
            let path_w = unicode_width::UnicodeWidthStr::width(state.git_root.as_str())
                .max(unicode_width::UnicodeWidthStr::width(state.cwd.as_str()))
                as u16
                + 14; // "  Git root: " / "  CWD:      " prefixes.
            let width = path_w.max(60).min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, 11, area);
            let inner =
                dialogs::render_dialog_frame("Mount Scope", Color::Yellow, dialog_area, frame);
            let lines: Vec<Line> = vec![
                Line::from(format!("  Git root: {}", state.git_root)),
                Line::from(format!("  CWD:      {}", state.cwd)),
                Line::from(""),
                Line::from("  [r] Mount git root"),
                Line::from("  [c] Mount current dir only"),
                Line::from(""),
                Line::from(Span::styled(
                    "  [a / Esc] Abort",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::AgentAuth(state) => {
            let max_var_w = state
                .env_vars
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
                .max()
                .unwrap_or(0) as u16
                + 8;
            let agent_w =
                unicode_width::UnicodeWidthStr::width(state.agent_name.as_str()) as u16 + 12;
            let width = max_var_w
                .max(agent_w)
                .max(55)
                .min(area.width.saturating_sub(4));
            let height = (state.env_vars.len() as u16 + 8)
                .min(area.height.saturating_sub(4))
                .max(9);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(
                "Agent credentials?",
                Color::Yellow,
                dialog_area,
                frame,
            );
            let mut lines = vec![
                Line::from(format!("  Agent: {}", state.agent_name)),
                Line::from("  Env vars to inject:"),
            ];
            for var in &state.env_vars {
                lines.push(Line::from(format!("    - {var}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [y] Accept   [n] Decline   [o] Decline once   [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::ConfigShow(state) => {
            render_config_show(state, area, frame);
        }
        dialogs::Dialog::Loading { title } => {
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = title_w.max(40).min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(width, 6, area);
            let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            frame.render_widget(
                Paragraph::new("  Loading...").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
        dialogs::Dialog::WorkflowStepConfirm(state) => {
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
            let inner =
                dialogs::render_dialog_frame("Step Complete", Color::Green, dialog_area, frame);
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
        dialogs::Dialog::Custom { title, body, keys } => {
            let body_lines = body.lines().count() as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            // Use display width, not byte length, so wide chars/emoji size
            // the dialog correctly. Account for padding + borders.
            let max_body_width = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(40) as u16;
            let max_key_label_width = keys
                .iter()
                .map(|(_, l)| unicode_width::UnicodeWidthStr::width(l.as_str()) + 6)
                .max()
                .unwrap_or(0) as u16;
            let width = max_body_width
                .max(max_key_label_width)
                .max(title_w)
                .saturating_add(6)
                .clamp(55, area.width.saturating_sub(4));
            let height = (keys.len() as u16 + body_lines + 7)
                .min(area.height.saturating_sub(2))
                .max(9);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines: Vec<Line> = body.lines().map(Line::from).collect();
            lines.push(Line::from(""));
            for (ch, label) in keys {
                lines.push(Line::from(format!("  [{ch}] {label}")));
            }
            // Always offer an Esc hint at the bottom — Custom is also used
            // for prompts where the natural cancel key is Esc.
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
    }
}

/// Render the config show dialog using a Ratatui `Table` widget.
fn render_config_show(state: &dialogs::ConfigShowState, area: Rect, frame: &mut Frame) {
    let popup_width = area.width.saturating_sub(4).min(110);
    let popup_height = area.height.saturating_sub(4).min(26);
    let popup = dialogs::centered_fixed(popup_width, popup_height, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" amux config ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let bottom_height: u16 = if state.editing { 3 } else { 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(bottom_height)])
        .split(inner);
    let table_area = chunks[0];
    let hint_area = chunks[1];

    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from("Field").style(header_style),
        Cell::from("Global").style(header_style),
        Cell::from("Repo").style(header_style),
        Cell::from("Effective").style(header_style),
    ])
    .height(1);

    let rows: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let is_selected = i == state.selected;

            let gval = if is_selected && state.editing && state.edit_column == 0 {
                let ev = &state.editor.text;
                let cursor = state.editor.cursor;
                format!("{}|{}", &ev[..cursor], &ev[cursor..])
            } else {
                row.global.clone()
            };
            let rval = if is_selected && state.editing && state.edit_column == 1 {
                let ev = &state.editor.text;
                let cursor = state.editor.cursor;
                format!("{}|{}", &ev[..cursor], &ev[cursor..])
            } else {
                row.repo.clone()
            };

            let (gcell, rcell) = if is_selected && !state.editing {
                let col_style = Style::default().fg(Color::Black).bg(Color::White);
                if state.edit_column == 0 {
                    (Cell::from(gval).style(col_style), Cell::from(rval))
                } else {
                    (Cell::from(gval), Cell::from(rval).style(col_style))
                }
            } else if is_selected && state.editing {
                let edit_style = Style::default().fg(Color::Black).bg(Color::Green);
                if state.edit_column == 0 {
                    (Cell::from(gval).style(edit_style), Cell::from(rval))
                } else {
                    (Cell::from(gval), Cell::from(rval).style(edit_style))
                }
            } else {
                (Cell::from(gval), Cell::from(rval))
            };

            let r = Row::new(vec![
                Cell::from(row.field.as_str()),
                gcell,
                rcell,
                Cell::from(row.effective.as_str()),
            ]);
            if is_selected {
                r.style(Style::default().fg(Color::White).bg(Color::DarkGray))
            } else if row.read_only {
                r.style(Style::default().fg(Color::DarkGray))
            } else {
                r
            }
        })
        .collect();

    let widths = [
        Constraint::Percentage(28),
        Constraint::Percentage(24),
        Constraint::Percentage(24),
        Constraint::Percentage(24),
    ];
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, table_area);

    let mut hint_lines: Vec<Line> = Vec::new();
    if state.editing {
        hint_lines.push(Line::from(vec![
            Span::styled("  Editing", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw("=save  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw("=cancel"),
        ]));
    } else {
        hint_lines.push(Line::from(vec![
            Span::styled("  \u{2191}\u{2193}", Style::default().fg(Color::Yellow)),
            Span::raw("=row  "),
            Span::styled("\u{2190}\u{2192}", Style::default().fg(Color::Yellow)),
            Span::raw("=col  "),
            Span::styled("e", Style::default().fg(Color::Yellow)),
            Span::raw("=edit  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw("=close"),
        ]));
    }
    frame.render_widget(Paragraph::new(hint_lines), hint_area);
}

#[cfg(test)]
mod tests {
    use super::truncate_middle;

    #[test]
    fn long_path_truncated_with_middle_ellipsis() {
        let long_path = "/home/user/projects/very-long-directory-name/another-long-part/file.txt";
        let result = truncate_middle(long_path, 30);
        assert!(
            result.contains('\u{2026}'),
            "long path must be truncated with '…', got: {result:?}"
        );
        assert!(
            result.chars().count() <= 30,
            "truncated string must be at most 30 chars, got {} chars: {result:?}",
            result.chars().count()
        );
    }

    #[test]
    fn short_path_not_truncated() {
        let short = "/home/user/foo";
        let result = truncate_middle(short, 40);
        assert_eq!(result, short, "path shorter than max must not be truncated");
    }

    #[test]
    fn truncate_middle_exact_length_not_truncated() {
        let s = "abcdefghij"; // 10 chars
        let result = truncate_middle(s, 10);
        assert_eq!(
            result, s,
            "string at exactly max chars must not be truncated"
        );
    }

    #[test]
    fn truncate_middle_preserves_prefix_and_suffix() {
        let s = "start-middle-end";
        let result = truncate_middle(s, 10);
        assert!(result.starts_with("star"), "prefix must be preserved");
        assert!(result.ends_with("end"), "suffix must be preserved");
        assert!(result.contains('\u{2026}'));
    }
}
