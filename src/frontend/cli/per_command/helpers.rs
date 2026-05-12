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
    eprintln!("amux: {prompt} {suffix}");
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
    eprintln!("amux: {prompt}");
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
    eprintln!("amux: {prompt}");
    eprintln!("amux: (enter a blank line or press Ctrl+D when done)");
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

/// Render a [`StepStatus`] as a short human label suitable for inline progress
/// lines (e.g. `Build base image: running`).
pub fn step_status_label(status: &StepStatus) -> String {
    match status {
        StepStatus::Pending => "pending".to_string(),
        StepStatus::Running => "running".to_string(),
        StepStatus::Done => "done".to_string(),
        StepStatus::Skipped => "skipped".to_string(),
        StepStatus::Warn(msg) if msg.is_empty() => "warn".to_string(),
        StepStatus::Warn(msg) => format!("warn: {msg}"),
        StepStatus::Failed(reason) if reason.is_empty() => "failed".to_string(),
        StepStatus::Failed(reason) => format!("failed: {reason}"),
    }
}

/// Render a [`StepStatus`] as a single glyph for summary tables.
/// `-` Pending, `…` Running, `✓` Done, `–` Skipped, `⚠` Warn, `✗` Failed.
pub fn step_status_glyph(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "-",
        StepStatus::Running => "…",
        StepStatus::Done => "✓",
        StepStatus::Skipped => "–",
        StepStatus::Warn(_) => "⚠",
        StepStatus::Failed(_) => "✗",
    }
}

/// Build an ASCII summary box with a title and label/status rows. Mirrors the
/// `Init Summary` / `Ready Summary` boxes from the legacy CLI.
pub fn render_summary_box(title: &str, rows: &[(&str, &StepStatus)]) -> String {
    let label_w = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(8)
        .max(16);
    // Value column carries glyph + space + label.
    let value_w = rows
        .iter()
        .map(|(_, s)| step_status_label(s).chars().count() + 2)
        .max()
        .unwrap_or(10)
        .max(12);
    let table_inner = label_w + value_w + 5; // " label │ value " + borders
    let title_inner = title.chars().count() + 2; // " title "
    let inner = table_inner.max(title_inner);
    let value_w = if inner > table_inner {
        value_w + (inner - table_inner)
    } else {
        value_w
    };

    let mut out = String::new();
    out.push_str(&format!("┌{}┐\n", "─".repeat(inner)));
    let title_pad = inner.saturating_sub(title.chars().count() + 2);
    out.push_str(&format!("│ {}{} │\n", title, " ".repeat(title_pad)));
    out.push_str(&format!(
        "├{}┬{}┤\n",
        "─".repeat(label_w + 2),
        "─".repeat(value_w + 2)
    ));
    for (label, status) in rows {
        let label_pad = label_w.saturating_sub(label.chars().count());
        let value = format!(
            "{} {}",
            step_status_glyph(status),
            step_status_label(status)
        );
        let value_pad = value_w.saturating_sub(value.chars().count());
        out.push_str(&format!(
            "│ {}{} │ {}{} │\n",
            label,
            " ".repeat(label_pad),
            value,
            " ".repeat(value_pad)
        ));
    }
    out.push_str(&format!(
        "└{}┴{}┘\n",
        "─".repeat(label_w + 2),
        "─".repeat(value_w + 2)
    ));
    out
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
