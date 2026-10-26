//! Dialogs that belong to no larger family: setup and consent prompts,
//! notices, the fatal-error modal.
//!
//! Split out of `render/dialog.rs` by WI 0114 F-51: one `render_*` per
//! dialog variant, so the 1,700-line `match` in `render_dialog` is one call
//! per arm.

use super::super::*;
use crate::frontend::tui::dialogs;

pub(crate) fn render_agent_setup(state: &dialogs::AgentSetupState, area: Rect, frame: &mut Frame) {
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

pub(crate) fn render_mount_scope(state: &dialogs::MountScopeState, area: Rect, frame: &mut Frame) {
    // Paths can be long — auto-grow to fit, but cap to area.
    let path_w = unicode_width::UnicodeWidthStr::width(state.git_root.as_str())
        .max(unicode_width::UnicodeWidthStr::width(state.cwd.as_str())) as u16
        + 14; // "  Git root: " / "  CWD:      " prefixes.
    let width = path_w.max(60).min(area.width.saturating_sub(4));
    let dialog_area = dialogs::centered_fixed(width, 11, area);
    let inner = dialogs::render_dialog_frame("Mount Scope", Color::Yellow, dialog_area, frame);
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

pub(crate) fn render_agent_auth(state: &dialogs::AgentAuthState, area: Rect, frame: &mut Frame) {
    let max_var_w = state
        .env_vars
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
        .max()
        .unwrap_or(0) as u16
        + 8;
    let agent_w = unicode_width::UnicodeWidthStr::width(state.agent_name.as_str()) as u16 + 12;
    let width = max_var_w
        .max(agent_w)
        .max(55)
        .min(area.width.saturating_sub(4));
    let height = (state.env_vars.len() as u16 + 8)
        .min(area.height.saturating_sub(4))
        .max(9);
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let inner =
        dialogs::render_dialog_frame("Agent credentials?", Color::Yellow, dialog_area, frame);
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

pub(crate) fn render_loading(title: &str, area: Rect, frame: &mut Frame) {
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
    let width = title_w.max(40).min(area.width.saturating_sub(4));
    let dialog_area = dialogs::centered_fixed(width, 6, area);
    let inner = dialogs::render_dialog_frame(title, Color::Cyan, dialog_area, frame);
    frame.render_widget(
        Paragraph::new("  Loading...").style(Style::default().fg(Color::DarkGray)),
        inner,
    );
}

pub(crate) fn render_custom(
    title: &str,
    body: &str,
    keys: &[(char, String)],
    area: Rect,
    frame: &mut Frame,
) {
    let body_lines = body.lines().count() as u16;
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
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
        .max(55)
        .min(area.width.saturating_sub(4));
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

pub(crate) fn render_fatal_error(title: &str, body: &str, area: Rect, frame: &mut Frame) {
    let body_lines = body.lines().count() as u16;
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
    let max_body_width = body
        .lines()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(40) as u16;
    let width = max_body_width
        .max(title_w)
        .saturating_add(6)
        .max(55)
        .min(area.width.saturating_sub(4));
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

pub(crate) fn render_notice(
    title: &str,
    body: &str,
    copy_key: Option<&String>,
    copy_zshrc_snippet: Option<&String>,
    area: Rect,
    frame: &mut Frame,
) {
    let body_lines = body.lines().count() as u16;
    let title_w = unicode_width::UnicodeWidthStr::width(title) as u16 + 4;
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
        .max(55)
        .min(area.width.saturating_sub(4));
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

/// Hard-wrap `line` at `width` display columns, keeping every character
/// (spaces included) so a reviewed argv reads exactly as it will run.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_w = 0;
    for ch in line.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + ch_w > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(ch);
        current_w += ch_w;
    }
    out.push(current);
    out
}

/// The trust-review dialog: the body is wrapped to the dialog's width and
/// scrolls under a fixed footer, so the choices and the scroll position are
/// always visible however long the review is or however small the terminal.
/// A gated choice is shown as unavailable until the end has been on screen.
pub(crate) fn render_review(state: &dialogs::ReviewState, area: Rect, frame: &mut Frame) {
    let title_w = unicode_width::UnicodeWidthStr::width(state.title.as_str()) as u16 + 4;
    let body_w = state
        .body
        .lines()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .min(100) as u16;
    let width = body_w
        .saturating_add(6)
        .max(title_w)
        .max(60)
        .min(area.width.saturating_sub(4));
    // Status row, blank row, one row per key, Esc row.
    let footer_rows = state.keys.len() as u16 + 3;
    let text_w = width.saturating_sub(4).max(1) as usize;
    let wrapped: Vec<String> = state
        .body
        .lines()
        .flat_map(|line| hard_wrap(line, text_w))
        .collect();
    let wanted = (wrapped.len().min(u16::MAX as usize) as u16)
        .saturating_add(footer_rows)
        .saturating_add(2);
    let height = wanted.min(area.height.saturating_sub(2));
    let dialog_area = dialogs::centered_fixed(width, height, area);
    let inner = dialogs::render_dialog_frame(&state.title, Color::Yellow, dialog_area, frame);
    let footer_h = footer_rows.min(inner.height);
    let body_area = Rect {
        height: inner.height - footer_h,
        ..inner
    };
    let footer_area = Rect {
        y: inner.y + body_area.height,
        height: footer_h,
        ..inner
    };

    // With no room for the body at all, nothing can be reviewed: the end is
    // never "seen", so a gated choice stays unavailable.
    let max_scroll = if body_area.height == 0 {
        usize::MAX
    } else {
        wrapped.len().saturating_sub(body_area.height as usize)
    };
    state.max_scroll.set(max_scroll);
    let scroll = state.scroll.min(max_scroll).min(wrapped.len());
    let visible: Vec<Line> = wrapped
        .iter()
        .skip(scroll)
        .take(body_area.height as usize)
        .map(|line| Line::from(line.clone()))
        .collect();
    frame.render_widget(Paragraph::new(visible), body_area);

    let dim = Style::default().fg(Color::DarkGray);
    let shown_to = (scroll + body_area.height as usize).min(wrapped.len());
    let status = if body_area.height == 0 {
        Span::styled(
            "  ⚠ terminal too small to show the review",
            Style::default().fg(Color::Red),
        )
    } else if state.fully_seen() {
        Span::styled(
            format!(
                "  end of review — lines {}–{shown_to} of {}",
                scroll + 1,
                wrapped.len()
            ),
            dim,
        )
    } else {
        Span::styled(
            format!(
                "  ▼ more below — ↑/↓/PgDn to scroll (lines {}–{shown_to} of {})",
                scroll + 1,
                wrapped.len()
            ),
            Style::default().fg(Color::Yellow),
        )
    };
    let mut footer = vec![Line::from(status), Line::from("")];
    for (ch, label) in &state.keys {
        if state.accepts(*ch) {
            footer.push(Line::from(format!("  [{ch}] {label}")));
        } else {
            footer.push(Line::from(Span::styled(
                format!("  [{ch}] {label} (scroll to the end first)"),
                dim,
            )));
        }
    }
    footer.push(Line::from(Span::styled("  [Esc] cancel", dim)));
    frame.render_widget(Paragraph::new(footer), footer_area);
}
