//! Squad modals: task detail, run history, and the four confirmations.
//!
//! Split out of `render/dialog.rs` by WI 0114 F-51: one `render_*` per
//! dialog variant, so the 1,700-line `match` in `render_dialog` is one call
//! per arm.

use super::super::*;
use crate::command::commands::squad::commands::SquadConfirmDecision;
use crate::data::prompt::Prompt;
use crate::frontend::tui::dialogs;

pub(crate) fn render_start_confirm(area: Rect, frame: &mut Frame) {
    let width = 66u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = dialogs::centered_fixed(width, 9, area);
    let inner =
        dialogs::render_dialog_frame("Start squad daemon?", Color::Cyan, dialog_area, frame);
    let text = "  The squad daemon is not running.\n\
                \n  Start it in the background and open the squad tab?\n\
                \n  [y] start   [n / Esc] cancel";
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

pub(crate) fn render_key_missing(area: Rect, frame: &mut Frame) {
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
    let inner =
        dialogs::render_dialog_frame("squad authentication", Color::Yellow, dialog_area, frame);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(crate) fn render_remove_confirm(name: &str, area: Rect, frame: &mut Frame) {
    let width = 60u16.min(area.width.saturating_sub(4).max(40));
    let dialog_area = dialogs::centered_fixed(width, 8, area);
    let inner = dialogs::render_dialog_frame("Remove task", Color::Yellow, dialog_area, frame);
    let text = format!("  Remove task \"{name}\"?\n\n  [y] remove   [n / Esc] cancel");
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// Draw a squad task-action confirmation.
///
/// Every word comes from `prompt` (WI 0114 F-55): the frame's title, the
/// question, and each choice's label under its own hotkey. The frontend adds
/// the `[k]` brackets and the `/ Esc` on the dismissing choice — decoration
/// around copy it did not write.
pub(crate) fn render_action_confirm(
    prompt: &Prompt<SquadConfirmDecision>,
    area: Rect,
    frame: &mut Frame,
) {
    let question = prompt.body.as_str();
    let question_w = unicode_width::UnicodeWidthStr::width(question) as u16;
    // Sized to the question (+2 indent, +4 frame) so it stays on one
    // line wherever the terminal allows.
    let width = question_w
        .saturating_add(6)
        .max(60)
        .min(area.width.saturating_sub(4).max(40));
    let dialog_area = dialogs::centered_fixed(width, 8, area);
    let inner = dialogs::render_dialog_frame(&prompt.title, Color::Yellow, dialog_area, frame);
    let keys = prompt
        .choices
        .iter()
        .map(|choice| {
            // Esc answers whatever `default_on_dismiss` is, so it is advertised
            // beside that choice and nowhere else.
            if prompt.default_on_dismiss == Some(choice.value) {
                format!("[{} / Esc] {}", choice.key, choice.label)
            } else {
                format!("[{}] {}", choice.key, choice.label)
            }
        })
        .collect::<Vec<_>>()
        .join("   ");
    let text = format!("  {question}\n\n  {keys}");
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// Render the squad task-detail modal (WI 0102): the description block plus
/// the labelled field block. Reads only the dialog state, which
/// `tick_all_tabs` keeps in sync with the squad tab's snapshot.
///
/// Run history is *not* here — it has its own modal (`render_squad_history`,
/// reached with `h`), because a long description used to push it off the
/// bottom of this one. With the table gone, the description is free to take
/// whatever room the fixed field lines leave.
pub(crate) fn render_squad_detail(
    state: &dialogs::SquadDetailState,
    area: Rect,
    frame: &mut Frame,
) {
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
pub(crate) fn render_squad_history(
    state: &dialogs::SquadHistoryState,
    area: Rect,
    frame: &mut Frame,
) {
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
