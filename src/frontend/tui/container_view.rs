//! Container/PTY overlay rendering.
//!
//! Everything renders from `Tab::container_slots` — a plain containerized
//! command is a one-slot group, a parallel workflow group is N slots.
//! Render pieces:
//! - **Maximized overlay** (`render_container_maximized`): the focused slot
//!   in a centered overlay covering ~95% of the parent area. Shows the agent
//!   name and workflow step (left title), live container stats (right
//!   title), an optional scrollback indicator (top center), and a copy hint
//!   (bottom center) when the user has a selection. Cells are drawn into
//!   `frame.buffer_mut()` directly so cursor placement, wide chars,
//!   italic/inverse modifiers, and selection highlight all work.
//! - **Minimized bars** (`render_container_bars`): 3-row green rounded
//!   strips with `agent [step] | container | cpu | mem | t` — one per
//!   non-focused slot while the overlay is up, or one per slot when the
//!   whole group is minimized.
//! - **Summary** (`render_container_summary`): a 3-row dashed-border strip
//!   shown after the container exits, with averaged stats and the exit code.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::data::program_status::ProgramState;
use crate::frontend::tui::tabs::{
    format_duration, ContainerSlot, LastContainerSummary, Tab, TextSelection,
};

/// Render the container overlay when Maximized.
///
/// Mutates `tab` in two ways: stores the inner area into
/// `tab.container_inner_area` so `handle_mouse_event` can translate raw
/// terminal coords into vt100 cell coords; and temporarily mutates the
/// vt100 scrollback offset to render the user's chosen scrollback view.
///
/// `workflow_overview_height` is the number of rows occupied by the Workflow
/// Overview below the execution window — the container overlay must not
/// cover it.
pub fn render_container_maximized(
    tab: &mut Tab,
    outer_area: Rect,
    workflow_overview_height: u16,
    frame: &mut Frame,
) {
    // 95% of the execution window area (between tab bar and command box).
    // Tab bar = 3 rows at top, status bar + command box + suggestion = 5 rows at bottom.
    let top_reserved: u16 = 3;
    let bottom_reserved: u16 = 5 + workflow_overview_height;
    let exec_height = outer_area
        .height
        .saturating_sub(top_reserved + bottom_reserved);
    let exec_width = outer_area.width;

    let container_height = ((exec_height as u32 * 95 / 100) as u16).max(5);
    let container_width = ((exec_width as u32 * 95 / 100) as u16).max(10);
    let offset_x = (exec_width.saturating_sub(container_width)) / 2;
    let offset_y = top_reserved + (exec_height.saturating_sub(container_height)) / 2;
    let container_area = Rect {
        x: outer_area.x + offset_x,
        y: outer_area.y + offset_y,
        width: container_width,
        height: container_height,
    };

    // The focused slot owns the overlay; without one there is nothing to
    // render (the window state is Hidden whenever no slots exist).
    let Some(focused_slot) = tab.focused_slot() else {
        return;
    };

    frame.render_widget(Clear, container_area);

    // Title strings.
    let info = focused_slot.container_info.as_ref();
    // A setup/teardown step's container is titled with the step itself.
    let agent_name = info.map(|i| i.display_name()).unwrap_or("Agent");
    let is_phase_step = info.is_some_and(|i| i.phase_step_title.is_some());
    let runtime_label = if info.is_some_and(|i| i.sandboxed) {
        "sandboxed"
    } else {
        "containerized"
    };
    // While a workflow runs, show the step this container is executing: the
    // slot's own step (parallel group slots), otherwise the workflow's
    // current step (sequential steps run in the backbone slot, whose
    // step_name is empty). A setup/teardown step already names itself in
    // place of the agent, so it gets no suffix.
    let step_name: Option<String> = if is_phase_step {
        None
    } else {
        Some(focused_slot.step_name.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                tab.shared
                    .workflow
                    .snapshot()
                    .view
                    .and_then(|view| view.current_step)
            })
    };
    let left_title = match step_name {
        Some(step) => format!(
            " \u{1F512} {} ({}) \u{2014} {} ",
            agent_name, runtime_label, step
        ),
        None => format!(" \u{1F512} {} ({}) ", agent_name, runtime_label),
    };
    let right_title = info.map(build_stats_title_from_info).unwrap_or_default();
    let (left_title, right_title) =
        slot_titles_with_status(focused_slot, left_title, right_title, container_width);

    let mut block = Block::default()
        .title(Line::from(left_title).alignment(Alignment::Left))
        .title(Line::from(right_title).alignment(Alignment::Right))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green));

    // Probe vt100 for the effective offset and total scrollback depth.
    // vt100-ctt 0.17's `set_scrollback` clamps to the buffer length; its
    // `visible_rows()` uses `saturating_sub` for the live-rows portion
    // (the panic that vt100 0.15 had on offset > screen_rows is fixed),
    // so we can scroll the full configured `tui.scrollbackLines`
    // depth without crashing. We probe by setting the requested offset
    // and reading back the clamped value, then probe the depth via
    // `set_scrollback(usize::MAX)`. Reset to live before rendering.
    let focused_idx = tab.focused_slot_idx;
    let (effective_scroll_offset, max_scrollback) = if tab.container_scroll_offset > 0 {
        let screen = tab.container_slots[focused_idx].vt100_parser.screen_mut();
        screen.set_scrollback(tab.container_scroll_offset);
        let eff = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let depth = screen.scrollback();
        screen.set_scrollback(0);
        (eff, depth)
    } else {
        (0, 0)
    };

    if effective_scroll_offset > 0 {
        let scroll_hint = format!(
            " \u{2191} scrollback ({} / {} lines) ",
            effective_scroll_offset, max_scrollback
        );
        block = block.title(
            Line::from(Span::styled(
                scroll_hint,
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    }

    let selection = tab.mouse_selection.clone();
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

    // Publish the inner area for the mouse handler.
    tab.container_inner_area = Some(inner);

    let screen = tab.container_slots[focused_idx].vt100_parser.screen_mut();
    if effective_scroll_offset > 0 {
        screen.set_scrollback(effective_scroll_offset);
        render_vt100_screen(frame, screen, inner, selection.as_ref(), false);
        screen.set_scrollback(0);
    } else {
        render_vt100_screen(frame, screen, inner, selection.as_ref(), true);
    }
}

/// Render the post-exit container summary bar. Shown for the previous
/// containerized command after it exits; replaced when the user runs a new
/// command.
pub fn render_container_summary(summary: &LastContainerSummary, area: Rect, frame: &mut Frame) {
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

    // Distinctive dashed border for the summary bar.
    let border_set = ratatui::symbols::border::Set {
        top_left: "\u{256d}",
        top_right: "\u{256e}",
        bottom_left: "\u{2570}",
        bottom_right: "\u{256f}",
        horizontal_top: "\u{254c}",
        horizontal_bottom: "\u{254c}",
        vertical_left: "\u{2506}",
        vertical_right: "\u{2506}",
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

/// Number of terminal rows one minimized container bar occupies (a 3-row
/// rounded-border strip).
pub const PARALLEL_BAR_HEIGHT: u16 = 3;

/// Render the stacked minimized container status bars: one 3-row
/// rounded-border strip per slot, showing the agent, workflow step (when
/// any), container name, and live stats.
///
/// `skip_focused` selects the display mode: `true` while the focused slot is
/// shown maximized (it gets no bar — its info is in the overlay title);
/// `false` when the whole group is minimized and every slot renders as a bar.
/// Stuck slots get a `⚠` prefix and yellow color; slots in a yolo countdown
/// flash purple/yellow on a per-second parity.
pub fn render_container_bars(tab: &Tab, area: Rect, frame: &mut Frame, skip_focused: bool) {
    let mut row: u16 = 0;
    for (idx, slot) in tab.container_slots.iter().enumerate() {
        if skip_focused && idx == tab.focused_slot_idx {
            continue; // the focused slot is shown maximized, not as a bar
        }
        if row + PARALLEL_BAR_HEIGHT > area.height {
            break;
        }
        // Draw only stdio slots here; ACP slots are drawn by
        // `acp_view::render_acp_bars`. Both walk the slots in the same order
        // and advance one bar height per non-focused slot, so the two passes
        // tile a mixed group's bars without overlap. For an all-stdio tab this
        // is unchanged — every slot is drawn.
        if !slot.is_acp() {
            let bar_area = Rect::new(area.x, area.y + row, area.width, PARALLEL_BAR_HEIGHT);
            render_one_minimized_bar(slot, bar_area, frame);
        }
        row += PARALLEL_BAR_HEIGHT;
    }
}

fn render_one_minimized_bar(slot: &ContainerSlot, area: Rect, frame: &mut Frame) {
    let stuck = slot.stuck && slot.program_status.is_none();
    let reported_color = slot.program_status.as_ref().map(|s| s.state);
    let (color, prefix) = if stuck {
        // Yellow + ⚠ prefix for stuck bars.
        (Color::Yellow, "\u{26a0} ")
    } else if slot.yolo_mode {
        // Purple/yellow flash keyed off the same per-second parity the tab
        // header uses.
        let c = if slot.elapsed_secs().is_multiple_of(2) {
            Color::Magenta
        } else {
            Color::Yellow
        };
        (c, "")
    } else if reported_color == Some(ProgramState::Blocked) {
        (Color::Yellow, "")
    } else if reported_color == Some(ProgramState::Error) {
        (Color::Red, "")
    } else {
        (Color::Green, "")
    };

    // While yoloing (and not shown maximized — see `render_container_bars`'
    // `skip_focused`), show the same countdown the per-slot modal would, so
    // the user can see time-to-auto-advance without switching focus to it.
    let yolo_suffix = if slot.yolo_mode {
        slot.yolo_state
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|s| format!(" | Yolo in {}s", s.remaining_secs))
            .unwrap_or_default()
    } else {
        String::new()
    };

    // `🔒 {agent} [{step}] | {container} | {cpu} | {mem} | {dur}`, with the
    // `[step]` segment omitted for slots that aren't a parallel workflow
    // step (plain commands, sequential steps).
    let stats_title = slot
        .container_info
        .as_ref()
        .map(build_stats_title_from_info)
        .unwrap_or_default();
    let step_segment = if slot.step_name.is_empty() {
        String::new()
    } else {
        format!(" [{}]", slot.step_name)
    };
    let content = format!(
        "{}\u{1F512} {}{} | {}{}",
        prefix,
        slot.agent_name(),
        step_segment,
        stats_title.trim(),
        yolo_suffix
    );
    // The agent's own status and message, when it reports one.
    let content = match slot_status_text(slot, 60) {
        Some(status) => format!("{content} | {status}"),
        None => content,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color));

    let para = Paragraph::new(Line::from(vec![Span::styled(
        format!(" {}", content),
        Style::default().fg(color),
    )]))
    .block(block);

    frame.render_widget(para, area);
}

// ─── Internals ──────────────────────────────────────────────────────────

/// The agent's reported status as one line for the window title and the
/// minimized bar: `🔒 blocked: permission — allow rm?`, or, for a working
/// agent that has been quiet a while, `working · silent 12m`. At most
/// `max_chars` characters; the message is sanitised and truncated first.
/// `None` when the agent does not report (or has cleared its status).
pub(crate) fn slot_status_text(slot: &ContainerSlot, max_chars: usize) -> Option<String> {
    use crate::frontend::program_status_text::{display_msg, short_label, tab_glyph};
    let status = slot.program_status.as_ref()?;
    let head = match slot.silent_note() {
        Some(note) => note,
        None => {
            let glyph = tab_glyph(status);
            if glyph.is_empty() {
                short_label(status)
            } else {
                format!("{glyph} {}", short_label(status))
            }
        }
    };
    let head_len = head.chars().count();
    let msg_room = max_chars.saturating_sub(head_len + 3);
    // Not enough room for a useful slice of the message: show the state only.
    if msg_room < 8 {
        return Some(head);
    }
    match status.msg.as_deref().map(|m| display_msg(m, msg_room)) {
        Some(msg) if !msg.is_empty() => Some(format!("{head} \u{2014} {msg}")),
        _ => Some(head),
    }
}

/// Put the step's reported state beside its name. Container stats use the
/// remaining space; a narrow window drops them before hiding the state.
pub(crate) fn slot_titles_with_status(
    slot: &ContainerSlot,
    left_title: String,
    right_title: String,
    window_width: u16,
) -> (String, String) {
    let Some(head) = slot_status_text(slot, 0) else {
        return (left_title, right_title);
    };
    let inner_width = usize::from(window_width.saturating_sub(2));
    let base_width = UnicodeWidthStr::width(left_title.as_str());
    let min_status_width = UnicodeWidthStr::width(head.as_str()) + 3; // em dash, spaces
    let right_width = UnicodeWidthStr::width(right_title.as_str());
    let right_title = if base_width + min_status_width + right_width < inner_width {
        right_title
    } else {
        String::new()
    };
    let budget =
        inner_width.saturating_sub(base_width + UnicodeWidthStr::width(right_title.as_str()) + 4);
    let status = slot_status_text(slot, budget).unwrap_or(head);
    (format!("{left_title}\u{2014} {status} "), right_title)
}

/// Build the right-side stats title: `" {container} | {cpu} | {mem} | {dur} "`.
/// Falls back to placeholder values until the first stats sample arrives.
pub(crate) fn build_stats_title_from_info(
    info: &crate::frontend::tui::tabs::ContainerInfo,
) -> String {
    let elapsed = info.start_time.elapsed().as_secs();
    let time_str = format_duration(elapsed);
    if let Some(ref stats) = info.latest_stats {
        format!(
            " {} | {:.1}% | {:.0}MiB | {} ",
            stats.name, stats.cpu_percent, stats.memory_mb, time_str
        )
    } else if !info.container_name.is_empty() {
        format!(" {} | ... | ... | {} ", info.container_name, time_str)
    } else {
        format!(" ... | ... | {} ", time_str)
    }
}

/// Render the vt100 screen cell-by-cell into `frame.buffer_mut()`.
///
/// `selection` may highlight a contiguous range of cells via `Modifier::REVERSED`.
/// `show_cursor` controls whether the visible cursor is placed at the screen's
/// reported cursor position; pass `false` while viewing scrollback so the
/// cursor doesn't appear in stale content.
fn render_vt100_screen(
    frame: &mut Frame,
    screen: &vt100::Screen,
    area: Rect,
    selection: Option<&TextSelection>,
    show_cursor: bool,
) {
    let buf = frame.buffer_mut();
    let rows = area.height as usize;
    let cols = area.width as usize;
    let (screen_rows, screen_cols) = screen.size();
    let screen_rows = screen_rows as usize;
    let screen_cols = screen_cols as usize;

    let norm_sel = selection.map(normalize_selection);

    for row in 0..rows.min(screen_rows) {
        let mut col = 0;
        while col < cols.min(screen_cols) {
            let cell = screen.cell(row as u16, col as u16);
            let x = area.x + col as u16;
            let y = area.y + row as u16;

            if let Some(cell) = cell {
                let contents = cell.contents();
                let mut style = Style::default()
                    .fg(convert_vt100_color(cell.fgcolor()))
                    .bg(convert_vt100_color(cell.bgcolor()));
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
                if cell_in_selection(norm_sel, row as u16, col as u16) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let symbol = if contents.is_empty() {
                    " ".to_string()
                } else {
                    contents.to_string()
                };
                if let Some(buf_cell) = buf.cell_mut((x, y)) {
                    buf_cell.set_symbol(&symbol).set_style(style);
                }
            }
            col += 1;
        }
    }

    if show_cursor && !screen.hide_cursor() {
        let (cursor_row, cursor_col) = screen.cursor_position();
        let cx = area.x + cursor_col;
        let cy = area.y + cursor_row;
        if cx < area.x + area.width && cy < area.y + area.height {
            frame.set_cursor_position((cx, cy));
        }
    }
}

/// Normalize a selection into `((start_row, start_col), (end_row, end_col))`
/// with start ≤ end, regardless of drag direction.
/// Shared with the execution window's selection highlight in `render.rs`.
pub(crate) fn normalize_selection(s: &TextSelection) -> ((u16, u16), (u16, u16)) {
    let start = (s.start_row, s.start_col);
    let end = (s.end_row, s.end_col);
    if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
        (start, end)
    } else {
        (end, start)
    }
}

/// Whether the cell at `(row, col)` falls inside the normalized selection.
/// Shared with the execution window's selection highlight in `render.rs`.
#[inline]
pub(crate) fn cell_in_selection(
    norm_sel: Option<((u16, u16), (u16, u16))>,
    row: u16,
    col: u16,
) -> bool {
    let Some(((sr, sc), (er, ec))) = norm_sel else {
        return false;
    };
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

fn convert_vt100_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
