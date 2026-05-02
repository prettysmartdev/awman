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

/// Render a [`StepStatus`] as a short human label suitable for inline progress
/// lines (e.g. `Build base image: running`).
pub fn step_status_label(status: &StepStatus) -> String {
    match status {
        StepStatus::Pending => "pending".to_string(),
        StepStatus::Running => "running".to_string(),
        StepStatus::Done => "done".to_string(),
        StepStatus::Skipped => "skipped".to_string(),
        StepStatus::Failed(reason) if reason.is_empty() => "failed".to_string(),
        StepStatus::Failed(reason) => format!("failed: {reason}"),
    }
}

/// Render a [`StepStatus`] as a single glyph for summary tables.
/// `-` Pending, `…` Running, `✓` Done, `–` Skipped, `✗` Failed.
pub fn step_status_glyph(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "-",
        StepStatus::Running => "…",
        StepStatus::Done => "✓",
        StepStatus::Skipped => "–",
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
    let inner = label_w + value_w + 5; // " label │ value " + borders

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
