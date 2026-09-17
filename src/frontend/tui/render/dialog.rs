//! Modal dialog rendering: dispatch over the `Dialog` enum plus the
//! ConfigShow table widget and its cursor-windowing helper.

use super::*;

/// Render the currently active dialog.
pub(super) fn render_dialog(dialog: &dialogs::Dialog, area: Rect, frame: &mut Frame) {
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
            // `body_h + 6`: 4 rows of frame + a blank separator + the hint
            // row. See `dialogs::render_yes_no` — a smaller height clips the
            // key hints off the bottom of the dialog.
            let height = (body_h + 6).min(area.height.saturating_sub(2)).max(8);
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
        dialogs::Dialog::WorkflowControlBoard(state) => {
            let extra_reasons = [
                state.continue_unavailable_reason.is_some(),
                state.cancel_to_previous_unavailable_reason.is_some(),
                state.finish_workflow_unavailable_reason.is_some(),
                state.restart_unavailable_reason.is_some(),
            ]
            .iter()
            .filter(|x| **x)
            .count() as u16;
            let failed = !state.failure_lines.is_empty();
            let base_height: u16 = if state.can_finish { 14 } else { 12 };
            // A failure banner adds its detail lines plus a blank separator.
            let failure_height = if failed {
                state.failure_lines.len() as u16 + 2
            } else {
                0
            };
            // Width fits the longest reason line (+ left margin) when present;
            // otherwise the diamond layout's natural minimum is comfortable.
            let max_reason_w = [
                state.continue_unavailable_reason.as_deref(),
                state.cancel_to_previous_unavailable_reason.as_deref(),
                state.finish_workflow_unavailable_reason.as_deref(),
                state.restart_unavailable_reason.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(|s| unicode_width::UnicodeWidthStr::width(s) + 15)
            .max()
            .unwrap_or(0) as u16;
            let max_failure_w = state
                .failure_lines
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()) + 6)
                .max()
                .unwrap_or(0) as u16;
            let step_w =
                unicode_width::UnicodeWidthStr::width(state.step_name.as_str()) as u16 + 10;
            let width = max_reason_w
                .max(max_failure_w)
                .max(step_w)
                .max(56)
                .min(area.width.saturating_sub(4));
            let dialog_area = dialogs::centered_fixed(
                width,
                (base_height + extra_reasons + failure_height).min(area.height.saturating_sub(2)),
                area,
            );
            let (title, frame_colour) = if failed {
                ("Workflow Control — step failed", Color::Red)
            } else if state.can_dismiss {
                ("Workflow Control (step running)", Color::Yellow)
            } else {
                ("Workflow Control", Color::Yellow)
            };
            let inner = dialogs::render_dialog_frame(title, frame_colour, dialog_area, frame);

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
            // Restart is disabled in a parallel group unless this is the
            // focused container (WI-0096 §10).
            let restart_disabled = state.restart_unavailable_reason.is_some() || !state.can_restart;
            let (up_arrow_style, up_label_style) = if restart_disabled {
                (dimmed_style, dimmed_style)
            } else {
                (arrow_style, label_style)
            };

            let mut lines: Vec<Line> = vec![Line::from(vec![
                Span::raw(if failed { " Failed step: " } else { " Step: " }),
                Span::styled(&state.step_name, step_style),
            ])];
            if failed {
                let err_style = Style::default().fg(Color::Red);
                for line in &state.failure_lines {
                    lines.push(Line::from(Span::styled(format!("   {line}"), err_style)));
                }
            }
            lines.push(Line::from(""));
            // ↑ Restart (top of diamond)
            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::styled("\u{2191}", up_arrow_style),
                Span::styled(
                    if failed {
                        " Restart failed step"
                    } else {
                        " Restart current step"
                    },
                    up_label_style,
                ),
            ]));
            if let Some(ref reason) = state.restart_unavailable_reason {
                lines.push(Line::from(Span::styled(
                    format!("           {reason}"),
                    dimmed_style,
                )));
            }
            lines.push(Line::from(""));
            lines.extend([
                // ← Cancel to prev    → Next: new container
                Line::from(vec![
                    Span::styled("\u{2190}", left_arrow_style),
                    Span::styled(" Cancel to prev", left_label_style),
                    Span::raw("   "),
                    Span::styled("\u{2192}", right_arrow_style),
                    Span::styled(
                        format!(
                            " {}",
                            state
                                .launch_next_label
                                .as_deref()
                                .unwrap_or("Next: new container")
                        ),
                        right_label_style,
                    ),
                ]),
                Line::from(""),
                // ↓ Next: same container (bottom of diamond)
                Line::from(vec![
                    Span::raw("         "),
                    Span::styled("\u{2193}", down_arrow_style),
                    Span::styled(" Next: same container", down_label_style),
                ]),
            ]);
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
            } else if failed {
                lines.push(Line::from(Span::styled(
                    "  [^C] Cancel workflow   [Esc] Pause",
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
        dialogs::Dialog::SquadTaskDetail(state) => {
            render_squad_detail(state, area, frame);
        }
        dialogs::Dialog::SquadTaskHistory(state) => {
            render_squad_history(state, area, frame);
        }
        dialogs::Dialog::SquadStartConfirm => {
            let width = 66u16.min(area.width.saturating_sub(4).max(40));
            let dialog_area = dialogs::centered_fixed(width, 9, area);
            let inner = dialogs::render_dialog_frame(
                "Start squad daemon?",
                Color::Cyan,
                dialog_area,
                frame,
            );
            let text = "  The squad daemon is not running.\n\
                        \n  Start it in the background and open the squad tab?\n\
                        \n  [y] start   [n / Esc] cancel";
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::SquadKeyMissing => {
            let width = 74u16.min(area.width.saturating_sub(4).max(40));
            let lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    "  The squad daemon requires a key, and this session has none.",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("  AWMAN_SQUAD_KEY is not set here, and squad's key was shown"),
                Line::from("  only once when it was minted — only its hash is stored, so"),
                Line::from("  the key itself cannot be read back."),
                Line::from(""),
                Line::from("  Mint a new key and restart the squad daemon onto it?"),
                Line::from(Span::styled(
                    "  Any other shell still exporting the old key will stop working.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  [y] mint a new key and restart squad   [n / Esc] cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            // +4 for the frame's borders and padding.
            let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(2));
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(
                "squad authentication",
                Color::Yellow,
                dialog_area,
                frame,
            );
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::SquadRemoveConfirm { name } => {
            let width = 60u16.min(area.width.saturating_sub(4).max(40));
            let dialog_area = dialogs::centered_fixed(width, 8, area);
            let inner =
                dialogs::render_dialog_frame("Remove task", Color::Yellow, dialog_area, frame);
            let text = format!("  Remove task \"{name}\"?\n\n  [y] remove   [n / Esc] cancel");
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::SquadActionConfirm { action, name } => {
            let question = action.question(name);
            let question_w = unicode_width::UnicodeWidthStr::width(question.as_str()) as u16;
            // Sized to the question (+2 indent, +4 frame) so it stays on one
            // line wherever the terminal allows.
            let width = question_w
                .saturating_add(6)
                .max(60)
                .min(area.width.saturating_sub(4).max(40));
            let dialog_area = dialogs::centered_fixed(width, 8, area);
            let inner =
                dialogs::render_dialog_frame(action.title(), Color::Yellow, dialog_area, frame);
            let text = format!("  {question}\n\n  [y] {}   [n / Esc] back", action.verb());
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
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
            // for prompts where the natural cancel key is Esc. A single-key
            // Custom is an acknowledgement rather than a choice, so Enter
            // accepts it too (see `dialog_router::handle_dialog_submit`).
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                if keys.len() == 1 {
                    "  [Enter] continue   [Esc] cancel"
                } else {
                    "  [Esc] cancel"
                },
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::FatalError { title, body } => {
            let body_lines = body.lines().count() as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let max_body_width = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(40) as u16;
            let width = max_body_width
                .max(title_w)
                .saturating_add(6)
                .clamp(55, area.width.saturating_sub(4));
            let height = (body_lines + 6).min(area.height.saturating_sub(2)).max(8);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Red, dialog_area, frame);
            let mut lines: Vec<Line> = body.lines().map(Line::from).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Enter] quit",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::Notice {
            title,
            body,
            copy_key,
            copy_zshrc_snippet,
        } => {
            let body_lines = body.lines().count() as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            // The squad key snippet contains a box-drawn banner and an indented
            // export line; size to the widest line so neither wraps.
            let max_body_width = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(40) as u16;
            let width = max_body_width
                .max(title_w)
                .saturating_add(6)
                .clamp(55, area.width.saturating_sub(4));
            // One hint line always ("[Enter] dismiss"), plus one more for
            // each copy action this notice actually offers.
            let hint_lines = 1 + copy_key.is_some() as u16 + copy_zshrc_snippet.is_some() as u16;
            let height = (body_lines + 5 + hint_lines)
                .min(area.height.saturating_sub(2))
                .max(8);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let mut lines: Vec<Line> = body.lines().map(Line::from).collect();
            lines.push(Line::from(""));
            if copy_key.is_some() {
                lines.push(Line::from(Span::styled(
                    "  [c] copy key",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if copy_zshrc_snippet.is_some() {
                lines.push(Line::from(Span::styled(
                    "  [z] copy .zshrc snippet",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines.push(Line::from(Span::styled(
                "  [Enter] dismiss",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        }
    }
}

/// Render the squad task-detail modal (WI 0102): the description block plus
/// the labelled field block. Reads only the dialog state, which
/// `tick_all_tabs` keeps in sync with the squad tab's snapshot.
///
/// Run history is *not* here — it has its own modal (`render_squad_history`,
/// reached with `h`), because a long description used to push it off the
/// bottom of this one. With the table gone, the description is free to take
/// whatever room the fixed field lines leave.
fn render_squad_detail(state: &dialogs::SquadDetailState, area: Rect, frame: &mut Frame) {
    use crate::data::fs::task_store::{MountScope, TaskStatus};

    // The frame costs four rows and four columns (borders plus the padding
    // `render_dialog_frame` adds), so the content is laid out against the
    // budget first and the modal is then sized to what it actually holds —
    // no trailing band of empty rows where the run table used to be.
    const CHROME: u16 = 4;
    // WI 0106 Part 5: the action tooltip — the same per-task actions the list
    // view's footer hints at, scoped to this modal's task and actually wired
    // up (`dialog_router::handle_dialog_char`) so a user doesn't have to
    // close the modal to attach/pause/resume/remove. `h` is the way back to
    // the run history that used to sit below the fields.
    const HINTS: [&str; 9] = [
        "h history",
        "a attach",
        "e edit",
        "t trigger",
        "c cancel",
        "p pause",
        "r resume",
        "d delete",
        "esc close",
    ];
    // Wide enough to keep every hint on one row. Only a terminal too narrow
    // for that wraps them, between whole actions.
    let hint_w = pack_hint_segments(&HINTS, usize::MAX)
        .first()
        .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) as u16)
        .unwrap_or(0);
    let width = area
        .width
        .saturating_sub(4)
        .clamp(50, hint_w.saturating_add(CHROME).max(90));
    let max_height = area.height.saturating_sub(4).clamp(8, 30);
    let content_width = width.saturating_sub(CHROME);

    let c = &state.task;
    let mount = match c.mount_scope {
        MountScope::Cwd => "cwd",
        MountScope::GitRoot => "gitroot",
        MountScope::Directory => "directory (no worktree)",
    };
    let status = match c.status {
        TaskStatus::Active => "active",
        TaskStatus::Paused => "paused",
    };
    let mut fields: Vec<Line> = vec![
        squad_field_line("Status", status),
        squad_field_line("Mount scope", mount),
        squad_field_line(
            "Interval",
            &crate::frontend::tui::tabs::format_duration(c.interval_secs),
        ),
        squad_field_line("Agent", c.agent.as_deref().unwrap_or("(default)")),
        squad_field_line("Model", c.model.as_deref().unwrap_or("(default)")),
        squad_field_line("Workspace", &c.repo_scope.display().to_string()),
        squad_field_line(
            "Worktree",
            if c.uses_worktree() {
                "yes"
            } else {
                "no (mounted directly)"
            },
        ),
        squad_field_line(
            "Overlays",
            &if c.overlays.is_empty() {
                "(none)".to_string()
            } else {
                c.overlays.join(", ")
            },
        ),
        squad_field_line(
            "Created",
            &c.created_at.format("%Y-%m-%d %H:%M").to_string(),
        ),
        squad_field_line(
            "Updated",
            &c.updated_at.format("%Y-%m-%d %H:%M").to_string(),
        ),
    ];
    // WI 0116 §6b — the same standing marker the card carries, in the field
    // block that already reads as a labelled record. Absent when there is
    // nothing to say, so the modal is unchanged for a fully-covered task.
    if !c.unmet_env.is_empty() {
        fields.push(squad_field_line(
            "Env",
            &format!("\u{26a0} {} unmet", c.unmet_env.join(", ")),
        ));
    }
    let field_h = fields.len() as u16;

    let tooltip_lines = pack_hint_segments(&HINTS, content_width as usize);
    let tooltip_h = tooltip_lines.len() as u16;

    // The description is free text and often longer than the modal is wide, so
    // it renders as its own wrapped multi-line block rather than a single
    // clipped `label: value` line. It gets every row the fixed field lines and
    // the tooltip do not need, and is ellipsised only when it genuinely
    // outgrows the modal.
    let description_cap = max_height
        .saturating_sub(CHROME)
        .saturating_sub(field_h)
        .saturating_sub(1 + tooltip_h)
        .max(1) as usize;
    let description =
        squad_description_lines(&c.description, content_width as usize, description_cap);
    let description_h = description.len() as u16;

    // `+ 1 + tooltip_h`: a blank separator row and the action tooltip under
    // the fields.
    let height = (description_h + field_h + 1 + tooltip_h + CHROME).min(max_height);
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let title = format!("task: {}", state.name);
    let inner = dialogs::render_dialog_frame(&title, Color::Cyan, dialog_area, frame);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // The tooltip takes the last rows before anything else is laid out, so a
    // terminal too short for the whole field block clips a field rather than
    // the rows that say how to leave the modal.
    let (body, tooltip) = squad_body_and_hint_rows(inner, tooltip_h);
    let chunks = Layout::vertical([
        Constraint::Length(description_h),
        Constraint::Length(field_h),
        Constraint::Min(0),
    ])
    .split(body);
    frame.render_widget(Paragraph::new(description), chunks[0]);
    frame.render_widget(Paragraph::new(fields), chunks[1]);

    // If even the tooltip is squeezed, keep its last rows: that is where
    // `esc close` is.
    let skip = tooltip_lines.len().saturating_sub(tooltip.height as usize);
    let hint_style = Style::default().fg(Color::DarkGray);
    frame.render_widget(
        Paragraph::new(
            tooltip_lines
                .into_iter()
                .skip(skip)
                .map(|line| Line::from(Span::styled(line, hint_style)))
                .collect::<Vec<_>>(),
        ),
        tooltip,
    );
}

/// Pack key-hint segments (`"h history"`, …) into as few `·`-separated lines
/// as fit `width`, never splitting a segment across lines.
fn pack_hint_segments(segments: &[&str], width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    const SEPARATOR: &str = " \u{b7} ";
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for segment in segments {
        let joined_w = UnicodeWidthStr::width(current.as_str())
            + UnicodeWidthStr::width(SEPARATOR)
            + UnicodeWidthStr::width(*segment);
        if current.is_empty() {
            current.push_str(segment);
        } else if joined_w <= width {
            current.push_str(SEPARATOR);
            current.push_str(segment);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(segment);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Split a squad modal's inner area into its body and the `hint_rows` key-hint
/// rows pinned to the bottom. Reserving the hint rows up front is what keeps
/// them on screen when the body is taller than the terminal allows.
fn squad_body_and_hint_rows(inner: Rect, hint_rows: u16) -> (Rect, Rect) {
    let body = Rect {
        height: inner.height.saturating_sub(hint_rows),
        ..inner
    };
    let hint = Rect {
        y: inner.y + inner.height.saturating_sub(hint_rows),
        height: hint_rows.min(inner.height),
        ..inner
    };
    (body, hint)
}

/// Render the squad run-history modal: the scrollable run table for one task,
/// on its own so a long description in the detail modal can never push it out
/// of view. Esc either returns to the detail modal or closes back to the card
/// grid, depending on where `h` was pressed (`state.from_detail`).
fn render_squad_history(state: &dialogs::SquadHistoryState, area: Rect, frame: &mut Frame) {
    // Sized to the runs it has, up to what the terminal allows: a task with
    // three runs gets a three-row table, not a mostly-empty box. Anything past
    // the cap is reached by scrolling.
    const CHROME: u16 = 4;
    // Wide enough for the longest reason and error in the history, never
    // narrower than the old fixed 90 columns and never wider than the
    // terminal allows. Whatever still does not fit is shared between the text
    // columns in proportion to what each one wants.
    let max_width = area.width.saturating_sub(6).max(50);
    let width = squad_run_history_width(&state.runs)
        .saturating_add(CHROME)
        .clamp(90.min(max_width), max_width);
    let max_height = area.height.saturating_sub(4).clamp(6, 30);
    let table_h = if state.runs.is_empty() {
        1
    } else {
        // The table's header row plus one row per run.
        (state.runs.len().min(u16::MAX as usize) as u16).saturating_add(1)
    };
    // `+ 2`: a blank separator row and the key-hint row under the table.
    let height = table_h.saturating_add(2 + CHROME).min(max_height);
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let title = format!("run history: {}", state.name);
    let inner = dialogs::render_dialog_frame(&title, Color::Cyan, dialog_area, frame);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (body, hint) = squad_body_and_hint_rows(inner, 1);
    if state.runs.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "This task has not run yet.",
                Style::default().fg(Color::DarkGray),
            )),
            body,
        );
    } else {
        render_squad_run_history(&state.runs, state.scroll, body, frame);
    }

    let back = if state.from_detail {
        "esc back to detail"
    } else {
        "esc close"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("\u{2191}/\u{2193} scroll \u{b7} {back}"),
            Style::default().fg(Color::DarkGray),
        )),
        hint,
    );
}

/// A `label: value` line for the detail modal's field block.
fn squad_field_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

/// The detail modal's description block: the label line followed by the full
/// description word-wrapped to `width`, indented two cells, and capped at
/// `max_lines` total (an ellipsis line marks a capped description).
fn squad_description_lines(
    description: &str,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    const INDENT: &str = "  ";
    let wrap_width = width.saturating_sub(INDENT.len()).max(1);
    let mut lines = vec![Line::from(Span::styled(
        "Description:",
        Style::default().fg(Color::DarkGray),
    ))];
    let mut wrapped: Vec<String> = description
        .lines()
        .flat_map(|line| wrap_display_width(line, wrap_width))
        .collect();
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    let cap = max_lines.saturating_sub(1).max(1);
    let truncated = wrapped.len() > cap;
    wrapped.truncate(cap);
    if truncated {
        if let Some(last) = wrapped.last_mut() {
            *last = format!("{last}\u{2026}");
        }
    }
    lines.extend(
        wrapped
            .into_iter()
            .map(|line| Line::from(Span::raw(format!("{INDENT}{line}")))),
    );
    lines
}

/// Greedy word wrap by display width. A single word wider than `width` is
/// split mid-word rather than overflowing the modal.
fn wrap_display_width(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        let sep_w = if current.is_empty() { 0 } else { 1 };
        if current_w + sep_w + word_w <= width {
            if sep_w == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_w += sep_w + word_w;
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if word_w <= width {
            current.push_str(word);
            current_w = word_w;
        } else {
            // Split an over-wide word across as many lines as it needs.
            for ch in word.chars() {
                let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                if current_w + ch_w > width && !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    current_w = 0;
                }
                current.push(ch);
                current_w += ch_w;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// The run-history table's fixed-width columns: the two timestamps and the
/// status label. Every other column is free text sized from its content.
const SQUAD_HISTORY_TIME_W: u16 = 17;
const SQUAD_HISTORY_STATUS_W: u16 = 14;

/// Whether the history shows the "Unmet env" column.
///
/// WI 0116 §6e — the names that were unmet when each run started, recorded
/// on the run row and shown here so a run that behaved oddly last Tuesday
/// can still be explained. The column appears only when some run in the
/// history actually carries one: measured over every run rather than the
/// visible window, so scrolling never reshapes the table.
fn squad_history_shows_unmet(runs: &[crate::data::fs::task_store::Run]) -> bool {
    runs.iter().any(|r| !r.unmet_env.is_empty())
}

/// The history table's header labels, in column order.
fn squad_history_headers(show_unmet: bool) -> Vec<&'static str> {
    let mut headers = vec!["Started", "Status", "Reason", "Finished", "Error"];
    if show_unmet {
        headers.push("Unmet env");
    }
    headers
}

/// One run's cell text, in the same column order as [`squad_history_headers`].
fn squad_history_cells(run: &crate::data::fs::task_store::Run, show_unmet: bool) -> Vec<String> {
    use crate::data::fs::task_store::RunStatus;
    let status = match run.status {
        RunStatus::Running => "running",
        RunStatus::NotTriggered => "not triggered",
        RunStatus::WorkflowExecuted => "executed",
        RunStatus::Failed => "failed",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Canceled => "canceled",
    };
    let finished = run
        .finished_at
        .map(|f| f.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "\u{2014}".to_string());
    let mut cells = vec![
        run.started_at.format("%Y-%m-%d %H:%M").to_string(),
        status.to_string(),
        run.reason.clone().unwrap_or_else(|| "\u{2014}".to_string()),
        finished,
        run.error.clone().unwrap_or_default(),
    ];
    if show_unmet {
        cells.push(if run.unmet_env.is_empty() {
            "\u{2014}".to_string()
        } else {
            format!("\u{26a0} {}", run.unmet_env.join(", "))
        });
    }
    cells
}

/// How wide each history column wants to be: the fixed widths for the
/// timestamp and status columns, and for every free-text column the widest of
/// its header and its cells across the whole history (not just the visible
/// window, so scrolling never reshapes the table).
fn squad_history_column_wants(runs: &[crate::data::fs::task_store::Run]) -> Vec<u16> {
    use unicode_width::UnicodeWidthStr;
    let show_unmet = squad_history_shows_unmet(runs);
    let mut wants: Vec<u16> = squad_history_headers(show_unmet)
        .iter()
        .map(|header| UnicodeWidthStr::width(*header) as u16)
        .collect();
    for run in runs {
        for (want, cell) in wants.iter_mut().zip(squad_history_cells(run, show_unmet)) {
            let cell_w = UnicodeWidthStr::width(cell.as_str()).min(u16::MAX as usize) as u16;
            *want = (*want).max(cell_w);
        }
    }
    wants[0] = SQUAD_HISTORY_TIME_W;
    wants[1] = SQUAD_HISTORY_STATUS_W;
    wants[3] = SQUAD_HISTORY_TIME_W;
    wants
}

/// The table width that shows every cell in full: each column's want plus the
/// one-cell gap between columns.
fn squad_run_history_width(runs: &[crate::data::fs::task_store::Run]) -> u16 {
    let wants = squad_history_column_wants(runs);
    let gaps = wants.len().saturating_sub(1) as u16;
    wants
        .iter()
        .fold(gaps, |total, want| total.saturating_add(*want))
}

/// The run-history table (Started | Status | Reason | Finished | Error),
/// scrolled by `scroll` rows.
fn render_squad_run_history(
    runs: &[crate::data::fs::task_store::Run],
    scroll: usize,
    area: Rect,
    frame: &mut Frame,
) {
    let show_unmet = squad_history_shows_unmet(runs);
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let header = Row::new(
        squad_history_headers(show_unmet)
            .into_iter()
            .map(|label| Cell::from(label).style(header_style)),
    );
    let rows: Vec<Row> = runs
        .iter()
        .skip(scroll)
        .map(|r| {
            Row::new(
                squad_history_cells(r, show_unmet)
                    .into_iter()
                    .map(Cell::from),
            )
        })
        .collect();
    // Fixed columns keep their width; the free-text columns fill what is left
    // in proportion to how much text each one holds, so a long reason and a
    // long error share a narrow terminal instead of one starving the other.
    let widths: Vec<Constraint> = squad_history_column_wants(runs)
        .into_iter()
        .enumerate()
        .map(|(index, want)| match index {
            0 | 1 | 3 => Constraint::Length(want),
            _ => Constraint::Fill(want.max(1)),
        })
        .collect();
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, area);
}

/// Render `text` with a visible `|` cursor at byte offset `cursor`, windowed
/// to at most `max` characters so the cursor never scrolls out of a narrow
/// table cell. `…` marks clipped content on either side.
pub(super) fn cursor_window(text: &str, cursor: usize, max: usize) -> String {
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
pub(super) fn render_config_show(state: &dialogs::ConfigShowState, area: Rect, frame: &mut Frame) {
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
