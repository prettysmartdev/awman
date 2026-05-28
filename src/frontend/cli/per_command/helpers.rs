//! Shared helpers for CLI per-command frontend impls.

use crate::engine::step_status::StepStatus;

use super::super::output::stdin_is_tty;

/// Prompt the user with `[Y/n]` or `[y/N]` when stdin is a TTY.
/// Returns `default_yes` immediately when stdin is not a TTY.
pub fn yes_no(prompt: &str, default_yes: bool) -> bool {
    if !stdin_is_tty() {
        return default_yes;
    }
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprintln!("awman: {prompt} {suffix}");
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_err() {
        return default_yes;
    }
    match buf.trim() {
        "y" | "Y" => true,
        "n" | "N" => false,
        _ => default_yes,
    }
}

/// Read a single line from stdin when stdin is a TTY. Returns the trimmed
/// content. Returns `None` when stdin is not a TTY (so callers can fall back
/// to safe defaults).
pub fn read_line(prompt: &str) -> Option<String> {
    if !stdin_is_tty() {
        return None;
    }
    eprintln!("awman: {prompt}");
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_err() {
        return None;
    }
    Some(buf.trim().to_string())
}

/// Read multiple lines from stdin until a blank line or EOF (Ctrl+D).
/// Returns the collected text with embedded newlines. Returns `None` when
/// stdin is not a TTY.
pub fn read_multiline(prompt: &str) -> Option<String> {
    use std::io::BufRead as _;
    if !stdin_is_tty() {
        return None;
    }
    eprintln!("awman: {prompt}");
    eprintln!("awman: (enter a blank line or press Ctrl+D when done)");
    let stdin = std::io::stdin();
    let mut lines: Vec<String> = Vec::new();
    for line in stdin.lock().lines() {
        match line {
            Ok(l) if l.is_empty() => break,
            Ok(l) => lines.push(l),
            Err(_) => break,
        }
    }
    Some(lines.join("\n"))
}

pub fn step_status_label(status: &StepStatus) -> String {
    status.label()
}

pub fn step_status_glyph(status: &StepStatus) -> &'static str {
    status.glyph()
}

pub fn render_summary_box(title: &str, rows: &[(&str, &StepStatus)]) -> String {
    crate::data::step_status::render_summary_box(title, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::step_status::StepStatus;

    #[test]
    fn step_status_label_all_variants() {
        assert_eq!(step_status_label(&StepStatus::Pending), "pending");
        assert_eq!(step_status_label(&StepStatus::Running), "running");
        assert_eq!(step_status_label(&StepStatus::Done), "done");
        assert_eq!(step_status_label(&StepStatus::Skipped), "skipped");
        assert_eq!(
            step_status_label(&StepStatus::Failed(String::new())),
            "failed"
        );
        assert_eq!(
            step_status_label(&StepStatus::Failed("out of disk".into())),
            "failed: out of disk"
        );
    }

    #[test]
    fn step_status_glyph_all_variants() {
        assert_eq!(step_status_glyph(&StepStatus::Pending), "-");
        assert_eq!(step_status_glyph(&StepStatus::Running), "…");
        assert_eq!(step_status_glyph(&StepStatus::Done), "✓");
        assert_eq!(step_status_glyph(&StepStatus::Skipped), "–");
        assert_eq!(step_status_glyph(&StepStatus::Failed("".into())), "✗");
    }

    #[test]
    fn render_summary_box_contains_title_and_row_labels() {
        let failed = StepStatus::Failed("timeout".into());
        let rows: Vec<(&str, &StepStatus)> = vec![
            ("Base image", &StepStatus::Done),
            ("Audit", &StepStatus::Skipped),
            ("Build", &failed),
        ];
        let s = render_summary_box("Test Summary", &rows);
        assert!(s.contains("Test Summary"), "title must appear in box: {s}");
        assert!(s.contains("Base image"), "row label must appear: {s}");
        assert!(s.contains("Audit"), "row label must appear: {s}");
        assert!(s.contains("done"), "Done status must appear: {s}");
        assert!(s.contains("skipped"), "Skipped status must appear: {s}");
        assert!(s.contains("failed"), "Failed status must appear: {s}");
    }

    #[test]
    fn render_summary_box_has_border_characters() {
        let rows: Vec<(&str, &StepStatus)> = vec![("Step", &StepStatus::Done)];
        let s = render_summary_box("Box", &rows);
        assert!(s.contains('┌'), "must contain top-left corner");
        assert!(s.contains('┐'), "must contain top-right corner");
        assert!(s.contains('└'), "must contain bottom-left corner");
        assert!(s.contains('┘'), "must contain bottom-right corner");
    }
}
