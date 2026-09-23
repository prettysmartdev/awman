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
