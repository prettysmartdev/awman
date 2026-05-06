//! UI chrome rendering — frame layout, tab bar, execution window, status bar,
//! command box, suggestion row.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

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
    let has_summary_bar = !has_minimized_container
        && container_state == ContainerWindowState::Hidden
        && has_summary;

    let extra_bar_height = if has_minimized_container || has_summary_bar { 3 } else { 0 };

    let chunks = Layout::vertical([
        Constraint::Length(3),                          // tab bar
        Constraint::Min(5),                             // execution window
        Constraint::Length(extra_bar_height),           // minimized OR summary
        Constraint::Length(workflow_height),            // workflow strip
        Constraint::Length(1),                          // status bar
        Constraint::Length(3),                          // command box
        Constraint::Length(1),                          // suggestion row
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
        workflow_view::render_workflow_strip(&wf_state, chunks[3], frame);
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
fn render_output_content(
    tab: &tabs::Tab,
    area: Rect,
    frame: &mut Frame,
) {
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
                    (w + inner_width - 1) / inner_width
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
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Hidden,
        ) => vec![Span::styled(
            " Press Esc to deselect the window ",
            Style::default().fg(Color::Yellow),
        )],
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

    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let title = if focused { " command " } else { " command (inactive) " };

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

    let prefix = Span::styled("> ", Style::default().fg(Color::Cyan));
    let display_text = app.command_input.text.replace('\n', "\u{21b5}");
    let line = Line::from(vec![prefix, Span::raw(display_text)]);
    frame.render_widget(Paragraph::new(line), inner);

    if focused && app.active_dialog.is_none() {
        let text_before_cursor = &app.command_input.text[..app.command_input.cursor];
        let display_before = unicode_width::UnicodeWidthStr::width(
            text_before_cursor.replace('\n', "\u{21b5}").as_str(),
        );
        let cursor_x = area.x + 1 + 2 + display_before as u16;
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
    let show_suggestions =
        app.focus == Focus::CommandBox && !app.suggestion_row.is_empty();

    if show_suggestions {
        let mut spans: Vec<Span> = Vec::with_capacity(app.suggestion_row.len() * 2);
        for (i, s) in app.suggestion_row.iter().enumerate() {
            let sep = if i == 0 {
                Span::raw("  ")
            } else {
                Span::styled("  \u{00b7}  ", Style::default().fg(Color::DarkGray))
            };
            spans.push(sep);
            spans.push(Span::styled(s.as_str(), Style::default().fg(Color::Cyan)));
        }
        let para =
            Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
        return;
    }

    // Context fallback: project's working directory.
    let cwd_str = app
        .active_tab()
        .session
        .working_dir()
        .to_string_lossy()
        .into_owned();
    let para = Paragraph::new(Line::from(vec![
        Span::styled("  CWD: ", Style::default().fg(Color::DarkGray)),
        Span::styled(cwd_str, Style::default().fg(Color::DarkGray)),
    ]));
    frame.render_widget(para, area);
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
            let dialog_area = dialogs::centered_fixed(50, 9, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let text = format!("{body}\n\n  [y] Yes   [n] No   [Esc] Cancel");
            frame.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }),
                inner,
            );
        }
        dialogs::Dialog::TextInput {
            title,
            prompt,
            editor,
        } => {
            let dialog_area = dialogs::centered_fixed(60, 7, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            let display_text: String = editor.text.chars().take(inner.width.saturating_sub(3) as usize).collect();
            let text = format!("{prompt}\n> {}", display_text);
            frame.render_widget(Paragraph::new(text), inner);
            let text_before_cursor = &editor.text[..editor.cursor];
            let cursor_display_w = unicode_width::UnicodeWidthStr::width(text_before_cursor) as u16;
            let cursor_x = inner.x + 2 + cursor_display_w.min(inner.width.saturating_sub(3));
            let cursor_y = inner.y + 1;
            if cursor_x < inner.x + inner.width {
                frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            }
        }
        dialogs::Dialog::MultilineInput {
            title,
            prompt,
            editor,
        } => {
            let dialog_area = dialogs::centered_rect(60, 50, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            let text = format!("{prompt}\n{}", editor.text);
            frame.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }),
                inner,
            );
            let lines_before: Vec<&str> = editor.text[..editor.cursor].split('\n').collect();
            let last_line = lines_before.last().unwrap_or(&"");
            let cursor_display_w = unicode_width::UnicodeWidthStr::width(*last_line) as u16;
            let prompt_lines = prompt.lines().count() as u16 + 1;
            let cursor_x = inner.x + cursor_display_w.min(inner.width.saturating_sub(1));
            let cursor_y = inner.y + prompt_lines + (lines_before.len() as u16).saturating_sub(1);
            if cursor_x < inner.x + inner.width && cursor_y < inner.y + inner.height {
                frame.set_cursor_position(Position::new(cursor_x, cursor_y));
            }
        }
        dialogs::Dialog::ListPicker {
            title,
            items,
            selected,
        } => {
            let height = (items.len() as u16 + 4).min(area.height.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(50, height, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            let lines: Vec<Line> = items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let prefix = if i == *selected { "▸ " } else { "  " };
                    let style = if i == *selected {
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    Line::from(Span::styled(format!("{prefix}{item}"), style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::KindSelect { title, options } => {
            let height = (options.len() as u16 + 4).min(area.height.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(50, height, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let lines: Vec<Line> = options
                .iter()
                .enumerate()
                .map(|(i, (_key, label))| {
                    Line::from(format!("  [{}] {label}", i + 1))
                })
                .collect();
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
            let base_height: u16 = if state.can_finish { 15 } else { 13 };
            let dialog_area =
                dialogs::centered_fixed(52, base_height + extra_reasons, area);
            let inner = dialogs::render_dialog_frame(
                "Workflow Control",
                Color::Yellow,
                dialog_area,
                frame,
            );

            let arrow_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            let label_style = Style::default().fg(Color::White);
            let dimmed_style = Style::default().fg(Color::DarkGray);
            let step_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
            let cancel_style = Style::default().fg(Color::Red);

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
            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::styled("^C", cancel_style),
                Span::styled(" Cancel workflow execution", cancel_style),
            ]));
            if state.can_finish {
                lines.push(Line::from(""));
                let finish_style =
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("Ctrl+Enter", finish_style),
                    Span::styled(" Finish workflow", finish_style),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [d] Disable auto-advance   [a] Abort   [Esc] Pause",
                dimmed_style,
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::WorkflowStepError(state) => {
            let height = (state.error_lines.len() as u16 + 8).min(area.height.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(60, height, area);
            let inner = dialogs::render_dialog_frame(
                "Step failed",
                Color::Red,
                dialog_area,
                frame,
            );
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
            lines.push(Line::from("  [r] Retry   [q] Pause   [a] Abort"));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::WorkflowYoloCountdown(state) => {
            let dialog_area = dialogs::centered_fixed(50, 7, area);
            let inner = dialogs::render_dialog_frame(
                "Yolo Countdown",
                Color::Magenta,
                dialog_area,
                frame,
            );
            let text = format!(
                "  Step: {}\n  Auto-advancing in {}s\n\n  [Esc] Cancel",
                state.step_name, state.remaining_secs
            );
            frame.render_widget(Paragraph::new(text), inner);
        }
        dialogs::Dialog::AgentSetup(state) => {
            let title = if state.image_only {
                format!("Build {} image?", state.agent_name)
            } else {
                format!("Set up {}?", state.agent_name)
            };
            let dialog_area = dialogs::centered_fixed(55, 10, area);
            let inner =
                dialogs::render_dialog_frame(&title, Color::Yellow, dialog_area, frame);
            let mut lines = vec![Line::from(""), Line::from("  [y] Yes   [n] No")];
            if state.has_fallback {
                if let Some(ref fb) = state.fallback_name {
                    lines.push(Line::from(format!("  [f] Fallback to {fb}")));
                }
            }
            lines.push(Line::from("  [Esc] Abort"));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::MountScope(state) => {
            let dialog_area = dialogs::centered_fixed(60, 10, area);
            let inner =
                dialogs::render_dialog_frame("Mount Scope", Color::Yellow, dialog_area, frame);
            let text = format!(
                "  Git root: {}\n  CWD:      {}\n\n  [r] Mount git root\n  [c] Mount current dir only\n  [a] Abort",
                state.git_root, state.cwd
            );
            frame.render_widget(Paragraph::new(text), inner);
        }
        dialogs::Dialog::AgentAuth(state) => {
            let height = (state.env_vars.len() as u16 + 8).min(area.height.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(55, height, area);
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
            lines.push(Line::from("  [y] Accept   [n] Decline   [o] Decline once"));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        dialogs::Dialog::ConfigShow(state) => {
            render_config_show(state, area, frame);
        }
        dialogs::Dialog::Loading { title } => {
            let dialog_area = dialogs::centered_fixed(40, 5, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
            frame.render_widget(
                Paragraph::new("  Loading...").style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
        dialogs::Dialog::Custom { title, body, keys } => {
            let height = (keys.len() as u16 + 6).min(area.height.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(55, height, area);
            let inner =
                dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines = vec![Line::from(body.as_str()), Line::from("")];
            for (ch, label) in keys {
                lines.push(Line::from(format!("  [{ch}] {label}")));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}

/// Render the config show dialog (full-screen table).
fn render_config_show(
    state: &dialogs::ConfigShowState,
    area: Rect,
    frame: &mut Frame,
) {
    let dialog_area = dialogs::centered_rect(90, 80, area);
    let inner = dialogs::render_dialog_frame("Config", Color::Cyan, dialog_area, frame);

    let header = Line::from(vec![
        Span::styled(
            format!("{:<25} {:<20} {:<20} {:<20}", "Field", "Global", "Repo", "Effective"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let mut lines = vec![header, Line::from("")];
    for (i, row) in state.rows.iter().enumerate() {
        let is_selected = i == state.selected;
        let style = if row.read_only {
            Style::default().fg(Color::DarkGray)
        } else if is_selected {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let prefix = if is_selected { "▸ " } else { "  " };
        let text = format!(
            "{}{:<23} {:<20} {:<20} {:<20}",
            prefix, row.field, row.global, row.repo, row.effective
        );
        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}
