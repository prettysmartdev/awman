//! Dialogs that take something from the user: a line, a block of text, a
//! choice from a list, a config edit.
//!
//! Split out of `render/dialog.rs` by WI 0114 F-51: one `render_*` per
//! dialog variant, so the 1,700-line `match` in `render_dialog` is one call
//! per arm.

use super::super::*;
use crate::frontend::tui::dialogs;
use crate::frontend::tui::text_edit::TextEdit;

pub(crate) fn render_text_input(
    title: &str,
    prompt: &str,
    editor: &TextEdit,
    area: Rect,
    frame: &mut Frame,
) {
    // Layout: prompt (multi-line) + spacer + bordered input + spacer +
    // hint row. Width grows with terminal but caps at 80.
    //
    // The height is the sum of exactly those rows: `prompt_lines`
    // + 1 spacer + 3 (the bordered input) + 1 spacer + 1 hint = the
    // inner height, plus 4 for the frame's borders and padding. It was
    // one row short, so the `[Enter] submit / [Esc] cancel` hint fell
    // outside the dialog and never rendered at all — the one modal in
    // the task interview with no visible key bindings.
    let prompt_lines = prompt.lines().count() as u16;
    let dialog_h = prompt_lines + 10;
    let dialog_w = (area.width.saturating_sub(8)).clamp(50, 80);
    let dialog_area = dialogs::centered_fixed(dialog_w, dialog_h, area);
    let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
    let prompt_area = Rect {
        height: prompt_lines,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(prompt).style(Style::default().fg(Color::Gray)),
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
        // The New Tab dialog is the one place `Ctrl-S` opens the
        // squad tab (see the key handler's intercept, keyed off the
        // same title), so it is the one place the hint row says so.
        let hint = if title == dialogs::NEW_TAB_DIALOG_TITLE {
            "  [Enter] submit   [Esc] cancel   [Ctrl+S] open squad"
        } else {
            "  [Enter] submit   [Esc] cancel"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            hint_area,
        );
    }
    let text_before_cursor = &editor.text[..editor.cursor];
    let cursor_display_w = unicode_width::UnicodeWidthStr::width(text_before_cursor) as u16;
    let cursor_x = input_inner.x + cursor_display_w.min(input_inner.width.saturating_sub(1));
    let cursor_y = input_inner.y;
    if cursor_x < input_inner.x + input_inner.width {
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
}

pub(crate) fn render_multiline_input(
    title: &str,
    prompt: &str,
    editor: &TextEdit,
    area: Rect,
    frame: &mut Frame,
) {
    let dialog_area = dialogs::centered_rect(70, 60, area);
    let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);

    // Layout: prompt lines, 1-row gap, bordered textarea, 1-row gap, hint.
    let prompt_lines = prompt.lines().count() as u16;
    let prompt_area = Rect {
        height: prompt_lines,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(prompt).style(Style::default().fg(Color::Gray)),
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
            Paragraph::new("  [Ctrl+Enter / Ctrl+S] submit   [Enter] newline   [Esc] cancel")
                .style(Style::default().fg(Color::DarkGray)),
            hint_area,
        );
    }

    // Place the cursor at the correct visual position.
    let display_row = cursor_visual_row.saturating_sub(scroll_offset);
    let cx =
        textarea_inner.x + (cursor_visual_col as u16).min(textarea_inner.width.saturating_sub(1));
    let cy = textarea_inner.y + display_row as u16;
    if cx < textarea_inner.x + textarea_inner.width && cy < textarea_inner.y + textarea_inner.height
    {
        frame.set_cursor_position(Position::new(cx, cy));
    }
}

pub(crate) fn render_list_picker(
    title: &str,
    items: &[String],
    selected: usize,
    area: Rect,
    frame: &mut Frame,
) {
    // Width fits the longest item plus margin/prefix; height fits up
    // to all items plus a hint, capped to the terminal area.
    let max_item_w = items
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
        .max()
        .unwrap_or(0) as u16;
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
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
            let prefix = if i == selected { "▸ " } else { "  " };
            let style = if i == selected {
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

pub(crate) fn render_kind_select(
    title: &str,
    options: &[(String, String)],
    area: Rect,
    frame: &mut Frame,
) {
    let max_label_w = options
        .iter()
        .map(|(_k, l)| unicode_width::UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0) as u16;
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
    let width = (max_label_w + 12)
        .max(title_w)
        .max(50)
        .min(area.width.saturating_sub(4));
    // One row per option, a blank separator, and the hint row — plus
    // the frame's 4 rows. The old `options + 5` was two short and
    // clipped the `[1-9] select   [Esc] cancel` hint away.
    let body_h = options.len() as u16 + 2;
    let height = (body_h + 4).min(area.height.saturating_sub(2)).max(8);
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

/// Render `text` with a visible `|` cursor at byte offset `cursor`, windowed
/// to at most `max` characters so the cursor never scrolls out of a narrow
/// table cell. `…` marks clipped content on either side.
pub(crate) fn cursor_window(text: &str, cursor: usize, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let cursor = cursor.min(text.len());
    let mut with_cursor = String::with_capacity(text.len() + 1);
    with_cursor.push_str(&text[..cursor]);
    with_cursor.push('|');
    with_cursor.push_str(&text[cursor..]);
    let chars: Vec<char> = with_cursor.chars().collect();
    if chars.len() <= max {
        return with_cursor;
    }
    let cursor_idx = text[..cursor].chars().count();
    // Keep the cursor visible with a char of context to its right.
    let start = cursor_idx
        .saturating_sub(max.saturating_sub(2))
        .min(chars.len() - max);
    let mut window: Vec<char> = chars[start..start + max].to_vec();
    if start > 0 {
        window[0] = '\u{2026}';
    }
    if start + max < chars.len() {
        let last = window.len() - 1;
        window[last] = '\u{2026}';
    }
    window.into_iter().collect()
}

/// Render the config show dialog using a Ratatui `Table` widget.
///
/// The popup takes 90% of the terminal in both dimensions. The bottom pane
/// shows the full (wrapped) value of the selected row — or the inline editor
/// or the Ctrl+N add-mapping prompt — so long values are never lost to cell
/// truncation.
pub(crate) fn render_config_show(state: &dialogs::ConfigShowState, area: Rect, frame: &mut Frame) {
    let popup = dialogs::centered_rect(90, 90, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" awman config ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // ── Bottom pane content, computed first so its height can flex ──────
    let editing_value = state.editing && state.new_entry.is_none();
    let edit_buffer = || {
        let text = &state.editor.text;
        let cursor = state.editor.cursor.min(text.len());
        format!("{}|{}", &text[..cursor], &text[cursor..])
    };
    let (detail_text, format_hint): (String, Option<String>) = match &state.new_entry {
        Some(dialogs::NewMapEntryPhase::Key) => (
            format!("  New model mapping \u{2014} agent name: {}", edit_buffer()),
            Some("ASCII letters, digits, '-' and '_'".to_string()),
        ),
        Some(dialogs::NewMapEntryPhase::Value { key }) => (
            format!(
                "  dynamicWorkflows.agentsToModels.{key} = {}",
                edit_buffer()
            ),
            Some("comma-separated model names".to_string()),
        ),
        Some(dialogs::NewMapEntryPhase::GuidanceEntry) => (
            format!("  New guidance entry: {}", edit_buffer()),
            Some("a single instruction the leader must follow".to_string()),
        ),
        None => match state.rows.get(state.selected) {
            Some(row) if editing_value => {
                let scope = if state.edit_column == 0 {
                    "global"
                } else {
                    "repo"
                };
                (
                    format!("  {} ({scope}) = {}", row.field, edit_buffer()),
                    row.value_hint.clone(),
                )
            }
            Some(row) => {
                let full = if state.edit_column == 0 {
                    &row.global
                } else {
                    &row.repo
                };
                let full = if full.is_empty() {
                    &row.effective
                } else {
                    full
                };
                let text = if full.is_empty() {
                    format!("  {} (no value set)", row.field)
                } else {
                    format!("  {} = {full}", row.field)
                };
                let text = if row.read_only {
                    format!("{text}  [read-only]")
                } else {
                    text
                };
                (text, row.value_hint.clone())
            }
            None => (String::new(), None),
        },
    };

    // The detail pane grows with its content (wrapped at the popup width) up
    // to a third of the popup, so long values stay fully readable.
    let detail_width = inner.width.max(1) as usize;
    let detail_lines = detail_text.chars().count().div_ceil(detail_width).max(1) as u16;
    let max_detail = (inner.height / 3).max(2);
    let detail_height = detail_lines.clamp(2, max_detail);
    let hint_height: u16 = 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(detail_height),
            Constraint::Length(hint_height),
        ])
        .split(inner);
    let table_area = chunks[0];
    let detail_area = chunks[1];
    let hint_area = chunks[2];

    // Column widths mirror the `widths` constraints below (34/22/22/22),
    // minus the 3 single-cell gaps ratatui inserts between the 4 columns.
    // Cell values are truncated to these widths so long values don't
    // overflow; the Field column gets the largest share because dotted
    // names (dynamicWorkflows.agentsToModels.<agent>) are the longest.
    let usable = table_area.width.saturating_sub(3) as u32;
    let field_w = (usable * 34 / 100) as usize;
    let col_w = (usable * 22 / 100) as usize;

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

    // Window rows so the selection stays visible when the table has more
    // rows than fit (e.g. dynamicWorkflows.agentsToModels.* expansions),
    // mirroring the ListPicker windowing pattern above.
    let visible = (table_area.height.saturating_sub(1)) as usize;
    let start = state
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(state.rows.len().saturating_sub(visible));

    let rows: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, row)| {
            let is_selected = i == state.selected;
            let cell_editing = is_selected && editing_value;

            let gval = if cell_editing && state.edit_column == 0 {
                cursor_window(&state.editor.text, state.editor.cursor, col_w)
            } else {
                sidebar::truncate_path(&row.global, col_w)
            };
            let rval = if cell_editing && state.edit_column == 1 {
                cursor_window(&state.editor.text, state.editor.cursor, col_w)
            } else {
                sidebar::truncate_path(&row.repo, col_w)
            };

            let (gcell, rcell) = if is_selected && !state.editing {
                let col_style = Style::default().fg(Color::Black).bg(Color::White);
                if state.edit_column == 0 {
                    (Cell::from(gval).style(col_style), Cell::from(rval))
                } else {
                    (Cell::from(gval), Cell::from(rval).style(col_style))
                }
            } else if cell_editing {
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
                Cell::from(sidebar::truncate_path(&row.field, field_w)),
                gcell,
                rcell,
                Cell::from(sidebar::truncate_path(&row.effective, col_w)),
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
        Constraint::Percentage(34),
        Constraint::Percentage(22),
        Constraint::Percentage(22),
        Constraint::Percentage(22),
    ];
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, table_area);

    let detail_style = if state.editing {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(detail_text)
            .style(detail_style)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        detail_area,
    );

    // Hint pane: one line with the rejection reason (when the last save
    // attempt failed) or the field's expected value format, one line with
    // the active key bindings.
    let mut hint_lines: Vec<Line> = Vec::new();
    hint_lines.push(match &state.error {
        Some(reason) => Line::from(Span::styled(
            format!("  ✗ {reason}"),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            match format_hint {
                Some(hint) => format!("  {hint}"),
                None => String::new(),
            },
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
    });
    let key = |k: &str| Span::styled(k.to_string(), Style::default().fg(Color::Yellow));
    hint_lines.push(match &state.new_entry {
        Some(dialogs::NewMapEntryPhase::Key) => Line::from(vec![
            Span::styled("  New mapping", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=confirm agent  "),
            key("Esc"),
            Span::raw("=cancel"),
        ]),
        Some(dialogs::NewMapEntryPhase::Value { .. }) => Line::from(vec![
            Span::styled("  New mapping", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=save mapping  "),
            key("Esc"),
            Span::raw("=cancel"),
        ]),
        Some(dialogs::NewMapEntryPhase::GuidanceEntry) => Line::from(vec![
            Span::styled("  New guidance entry", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=save entry  "),
            key("Esc"),
            Span::raw("=cancel"),
        ]),
        None if state.editing => Line::from(vec![
            Span::styled("  Editing", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            key("Enter"),
            Span::raw("=save  "),
            key("Esc"),
            Span::raw("=cancel  "),
            key("\u{2190}\u{2192}"),
            Span::raw("=cursor  "),
            key("Home/End"),
            Span::raw("=jump"),
        ]),
        None => {
            // The Ctrl+N add-entry hint only makes sense on the agentsToModels
            // or guidance rows; elsewhere it is noise. The label differs so
            // users know what will be added.
            let selected_field = state.rows.get(state.selected).map(|r| r.field.as_str());
            let on_mapping_row = selected_field
                .map(|f| {
                    f == "dynamicWorkflows.agentsToModels"
                        || f.starts_with("dynamicWorkflows.agentsToModels.")
                })
                .unwrap_or(false);
            let on_guidance_row = selected_field
                .map(|f| {
                    f == "dynamicWorkflows.guidance" || f.starts_with("dynamicWorkflows.guidance.")
                })
                .unwrap_or(false);
            let mut spans = vec![
                key("  \u{2191}\u{2193}"),
                Span::raw("=row  "),
                key("PgUp/PgDn"),
                Span::raw("=page  "),
                key("\u{2190}\u{2192}"),
                Span::raw("=col  "),
                key("Enter/e"),
                Span::raw("=edit  "),
            ];
            if on_mapping_row {
                spans.push(key("Ctrl+N"));
                spans.push(Span::raw("=add model mapping  "));
            } else if on_guidance_row {
                spans.push(key("Ctrl+N"));
                spans.push(Span::raw("=add entry  "));
            }
            spans.push(key("Esc"));
            spans.push(Span::raw("=close"));
            Line::from(spans)
        }
    });
    frame.render_widget(Paragraph::new(hint_lines), hint_area);
}
