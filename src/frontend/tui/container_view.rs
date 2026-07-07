//! Container/PTY overlay rendering — ports the old-amux container window
//! to the new architecture.
//!
//! Three render modes:
//! - **Maximized** (`render_container_maximized`): a centered overlay that
//!   covers ~95% of the parent area. Shows the agent name (left title), live
//!   container stats (right title), an optional scrollback indicator (top
//!   center), and a copy hint (bottom center) when the user has a selection.
//!   Cells are drawn into `frame.buffer_mut()` directly so cursor placement,
//!   wide chars, italic/inverse modifiers, and selection highlight all work.
//! - **Minimized** (`render_container_minimized`): a 3-row green rounded
//!   strip below the execution window with `agent | container | cpu | mem | t`.
//! - **Summary** (`render_container_summary`): a 3-row dashed-border strip
//!   shown after the container exits, with averaged stats and the exit code.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::frontend::tui::tabs::{
    format_duration, LastContainerSummary, ParallelContainerSlot, Tab, TextSelection,
};

/// Render the container overlay when Maximized.
///
/// Mutates `tab` in two ways: stores the inner area into
/// `tab.container_inner_area` so `handle_mouse_event` can translate raw
/// terminal coords into vt100 cell coords; and temporarily mutates the
/// vt100 scrollback offset to render the user's chosen scrollback view.
///
/// `workflow_strip_height` is the number of rows occupied by the workflow
/// strip below the execution window — the container overlay must not
/// cover it.
pub fn render_container_maximized(
    tab: &mut Tab,
    outer_area: Rect,
    workflow_strip_height: u16,
    frame: &mut Frame,
) {
    // 95% of the execution window area (between tab bar and command box).
    // Tab bar = 3 rows at top, status bar + command box + suggestion = 5 rows at bottom.
    let top_reserved: u16 = 3;
    let bottom_reserved: u16 = 5 + workflow_strip_height;
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

    frame.render_widget(Clear, container_area);

    // Title strings.
    let focused_slot_idx = tab.has_multiple_slots().then_some(tab.focused_slot_idx);
    let focused_info = focused_slot_idx
        .and_then(|idx| tab.parallel_slots.get(idx))
        .and_then(|slot| slot.container_info.as_ref());
    let info = focused_info.or(tab.container_info.as_ref());
    let agent_name = info
        .map(|i| i.agent_display_name.as_str())
        .unwrap_or("Agent");
    let runtime_label = if info.is_some_and(|i| i.sandboxed) {
        "sandboxed"
    } else {
        "containerized"
    };
    let left_title = format!(" \u{1F512} {} ({}) ", agent_name, runtime_label);
    let right_title = info.map(build_stats_title_from_info).unwrap_or_default();

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
    // so we can scroll the full configured `terminal_scrollback_lines`
    // depth without crashing. We probe by setting the requested offset
    // and reading back the clamped value, then probe the depth via
    // `set_scrollback(usize::MAX)`. Reset to live before rendering.
    let (effective_scroll_offset, max_scrollback) = if tab.container_scroll_offset > 0 {
        let screen = if let Some(idx) = focused_slot_idx {
            tab.parallel_slots[idx].vt100_parser.screen_mut()
        } else {
            tab.vt100_parser.screen_mut()
        };
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

    let screen = if let Some(idx) = focused_slot_idx {
        tab.parallel_slots[idx].vt100_parser.screen_mut()
    } else {
        tab.vt100_parser.screen_mut()
    };
    if effective_scroll_offset > 0 {
        screen.set_scrollback(effective_scroll_offset);
        render_vt100_screen(frame, screen, inner, selection.as_ref(), false);
        screen.set_scrollback(0);
    } else {
        render_vt100_screen(frame, screen, inner, selection.as_ref(), true);
    }
}

/// Render the minimized container bar. A single 3-row green rounded strip
/// showing the agent name, container name, CPU, memory, and elapsed time.
pub fn render_container_minimized(tab: &Tab, area: Rect, frame: &mut Frame) {
    let agent_name = tab
        .container_info
        .as_ref()
        .map(|i| i.agent_display_name.as_str())
        .unwrap_or("Agent");
    let stats_title = build_stats_title(tab);

    let content = format!("\u{1F512} {} | {}", agent_name, stats_title.trim());

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

/// Render the stacked one-row minimized bars for a parallel workflow group
/// (WI-0096 §5). One row per non-focused slot; `area.height` is expected to
/// equal the number of minimized slots. Each row reads:
///   `[step_name] agent · duration · status_glyph`
/// Stuck slots get a `⚠` prefix and yellow color; slots in a yolo countdown
/// flash purple/yellow on a per-second parity.
pub fn render_parallel_minimized_bars(tab: &Tab, area: Rect, frame: &mut Frame) {
    let mut row: u16 = 0;
    for (idx, slot) in tab.parallel_slots.iter().enumerate() {
        if idx == tab.focused_slot_idx {
            continue; // the focused slot is shown maximized, not as a bar
        }
        if row >= area.height {
            break;
        }
        let bar_area = Rect::new(area.x, area.y + row, area.width, 1);
        render_one_minimized_bar(slot, bar_area, frame);
        row += 1;
    }
}

fn render_one_minimized_bar(slot: &ParallelContainerSlot, area: Rect, frame: &mut Frame) {
    let duration = format_duration(slot.elapsed_secs());
    let (color, prefix, glyph) = if slot.stuck {
        // Yellow + ⚠ prefix for stuck bars.
        (Color::Yellow, "\u{26a0} ", '\u{26a0}')
    } else if slot.yolo_mode {
        // Purple/yellow flash keyed off the same per-second parity the tab
        // header uses.
        let c = if slot.elapsed_secs().is_multiple_of(2) {
            Color::Magenta
        } else {
            Color::Yellow
        };
        (c, "", '\u{25cf}')
    } else {
        (Color::Green, "", '\u{25cf}')
    };
    let content = format!(
        "{}[{}] {} \u{00b7} {} \u{00b7} {}",
        prefix,
        slot.step_name,
        slot.agent_name(),
        duration,
        glyph
    );
    let para = Paragraph::new(Line::from(Span::styled(
        content,
        Style::default().fg(color),
    )));
    frame.render_widget(para, area);
}

// ─── Internals ──────────────────────────────────────────────────────────

/// Test-accessible wrapper for `build_stats_title`.
#[cfg(test)]
pub fn build_stats_title_for_test(tab: &Tab) -> String {
    build_stats_title(tab)
}

/// Build the right-side stats title: `" {container} | {cpu} | {mem} | {dur} "`.
/// Falls back to placeholder values until the first stats sample arrives.
fn build_stats_title(tab: &Tab) -> String {
    let info = match &tab.container_info {
        Some(i) => i,
        None => return String::new(),
    };
    build_stats_title_from_info(info)
}

fn build_stats_title_from_info(info: &crate::frontend::tui::tabs::ContainerInfo) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_in_selection_inside_single_row() {
        let sel = Some(((2, 5), (2, 10)));
        assert!(cell_in_selection(sel, 2, 5));
        assert!(cell_in_selection(sel, 2, 10));
        assert!(cell_in_selection(sel, 2, 7));
    }

    #[test]
    fn cell_in_selection_outside_single_row() {
        let sel = Some(((2, 5), (2, 10)));
        assert!(!cell_in_selection(sel, 2, 4));
        assert!(!cell_in_selection(sel, 2, 11));
        assert!(!cell_in_selection(sel, 1, 7));
        assert!(!cell_in_selection(sel, 3, 7));
    }

    #[test]
    fn cell_in_selection_multiple_rows() {
        let sel = Some(((2, 5), (4, 3)));
        // Start row: anything from start_col to end of row
        assert!(cell_in_selection(sel, 2, 5));
        assert!(cell_in_selection(sel, 2, 80));
        assert!(!cell_in_selection(sel, 2, 4));
        // Middle rows: any column
        assert!(cell_in_selection(sel, 3, 0));
        assert!(cell_in_selection(sel, 3, 79));
        // End row: anything from start of row to end_col
        assert!(cell_in_selection(sel, 4, 0));
        assert!(cell_in_selection(sel, 4, 3));
        assert!(!cell_in_selection(sel, 4, 4));
    }

    #[test]
    fn cell_in_selection_none_returns_false() {
        assert!(!cell_in_selection(None, 5, 5));
    }

    // ── WI-0096 minimized-bar rendering (E2E) ───────────────────────────────

    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
    use crate::frontend::tui::tabs::{ParallelContainerSlot, ParallelSlotEvent, Tab};

    fn make_test_session() -> Session {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    fn render_bars_to_text(tab: &Tab, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_parallel_minimized_bars(tab, area, frame);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn slot(name: &str) -> ParallelContainerSlot {
        ParallelContainerSlot::new(name.to_string(), "claude".to_string(), 1000)
    }

    #[test]
    fn minimized_bars_render_only_non_focused_slots_and_swap_on_ctrl_s() {
        let mut tab = Tab::new(make_test_session());
        tab.parallel_slots.push(slot("build"));
        tab.parallel_slots.push(slot("test"));
        tab.focused_slot_idx = 0; // "build" is maximized (no bar)

        // Only the non-focused slot renders a minimized status bar.
        let text = render_bars_to_text(&tab, 60, 4);
        assert!(
            text.contains("[test]"),
            "non-focused slot renders as a bar: {text}"
        );
        assert!(
            !text.contains("[build]"),
            "the focused (maximized) slot is not drawn as a bar: {text}"
        );

        // Ctrl-S swaps focus → now "build" is the minimized bar, "test" maximized.
        tab.cycle_focused_slot();
        let text = render_bars_to_text(&tab, 60, 4);
        assert!(
            text.contains("[build]"),
            "after swap 'build' is the bar: {text}"
        );
        assert!(
            !text.contains("[test]"),
            "after swap the focused 'test' has no bar: {text}"
        );
    }

    #[test]
    fn minimized_bar_disappears_when_its_slot_exits() {
        let mut tab = Tab::new(make_test_session());
        tab.parallel_slots.push(slot("build"));
        tab.parallel_slots.push(slot("test"));
        tab.focused_slot_idx = 0;

        let text = render_bars_to_text(&tab, 60, 4);
        assert!(text.contains("[test]"));

        // The background step exits → its slot is evicted and the bar is gone.
        tab.parallel_slot_events
            .lock()
            .unwrap()
            .push_back(ParallelSlotEvent::Exited {
                step_name: "test".to_string(),
            });
        tab.drain_parallel_slot_events();

        let text = render_bars_to_text(&tab, 60, 4);
        assert!(
            !text.contains("[test]"),
            "the exited slot's bar must disappear: {text}"
        );
    }

    #[test]
    fn minimized_stuck_bar_shows_warning_glyph() {
        let mut tab = Tab::new(make_test_session());
        tab.parallel_slots.push(slot("build"));
        let mut stuck = slot("test");
        stuck.stuck = true;
        tab.parallel_slots.push(stuck);
        tab.focused_slot_idx = 0;

        let text = render_bars_to_text(&tab, 60, 4);
        assert!(
            text.contains('\u{26a0}'),
            "a stuck minimized bar shows the ⚠ glyph: {text}"
        );
    }
}
