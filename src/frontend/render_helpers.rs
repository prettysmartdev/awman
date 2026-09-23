//! Rendering shared by more than one frontend.
//!
//! Layer 3, and only Layer 3. The summary box below used to live in
//! `src/data/step_status.rs` — a box renderer in the data layer — with a
//! pass-through wrapper in the CLI's helpers that the TUI reached across for
//! (WI 0114 F-23). Both frontends now import it from here, and
//! `src/frontend/tui` imports nothing from `src/frontend/cli`.

use crate::data::step_status::StepStatus;

/// The one-character status marker a summary row is drawn with.
pub fn step_glyph(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "-",
        StepStatus::Running => "\u{2026}",
        StepStatus::Done => "\u{2713}",
        StepStatus::Skipped => "\u{2013}",
        StepStatus::Warn(_) => "\u{26a0}",
        StepStatus::Failed(_) => "\u{2717}",
    }
}

/// A titled two-column box of `label → status` rows, sized to its content.
pub fn render_summary_box(title: &str, rows: &[(&str, &StepStatus)]) -> String {
    let label_w = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(8)
        .max(16);
    let value_w = rows
        .iter()
        .map(|(_, s)| s.label().chars().count() + 2)
        .max()
        .unwrap_or(10)
        .max(12);
    let table_inner = label_w + value_w + 5;
    let title_inner = title.chars().count() + 2;
    let inner = table_inner.max(title_inner);
    let value_w = if inner > table_inner {
        value_w + (inner - table_inner)
    } else {
        value_w
    };

    let mut out = String::new();
    out.push_str(&format!("\u{250c}{}\u{2510}\n", "\u{2500}".repeat(inner)));
    let title_pad = inner.saturating_sub(title.chars().count() + 2);
    out.push_str(&format!(
        "\u{2502} {}{} \u{2502}\n",
        title,
        " ".repeat(title_pad)
    ));
    out.push_str(&format!(
        "\u{251c}{}\u{252c}{}\u{2524}\n",
        "\u{2500}".repeat(label_w + 2),
        "\u{2500}".repeat(value_w + 2)
    ));
    for (label, status) in rows {
        let label_pad = label_w.saturating_sub(label.chars().count());
        let value = format!("{} {}", step_glyph(status), status.label());
        let value_pad = value_w.saturating_sub(value.chars().count());
        out.push_str(&format!(
            "\u{2502} {}{} \u{2502} {}{} \u{2502}\n",
            label,
            " ".repeat(label_pad),
            value,
            " ".repeat(value_pad)
        ));
    }
    out.push_str(&format!(
        "\u{2514}{}\u{2534}{}\u{2518}\n",
        "\u{2500}".repeat(label_w + 2),
        "\u{2500}".repeat(value_w + 2)
    ));
    out
}

/// Render a [`ReadySummary`](crate::data::ready_summary::ReadySummary) as the
/// box every frontend shows after `ready`.
///
/// The *rows* come from `ReadySummary::rows()` in Layer 0 — one ordered list,
/// credential classification included — so the CLI, the TUI and a remote
/// session can no longer show three different tables (F-23).
pub fn render_ready_summary(summary: &crate::data::ready_summary::ReadySummary) -> String {
    let rows = summary.rows();
    let borrowed: Vec<(&str, &StepStatus)> = rows
        .iter()
        .map(|(label, status)| (label.as_str(), status))
        .collect();
    render_summary_box(
        &format!("Ready Summary ({})", summary.runtime_name),
        &borrowed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_box_contains_its_title_and_every_row_label() {
        let rows: Vec<(&str, &StepStatus)> = vec![
            ("Dockerfile", &StepStatus::Done),
            ("Base image", &StepStatus::Pending),
        ];
        let s = render_summary_box("Test Summary", &rows);
        assert!(s.contains("Test Summary"));
        assert!(s.contains("Dockerfile"));
        assert!(s.contains("Base image"));
    }

    #[test]
    fn the_box_is_drawn_with_box_characters() {
        let rows: Vec<(&str, &StepStatus)> = vec![("A", &StepStatus::Done)];
        let s = render_summary_box("Box", &rows);
        assert!(s.contains('\u{250c}'));
        assert!(s.contains('\u{2518}'));
    }

    #[test]
    fn every_status_has_a_distinct_glyph() {
        let all = [
            StepStatus::Pending,
            StepStatus::Running,
            StepStatus::Done,
            StepStatus::Skipped,
            StepStatus::Warn(String::new()),
            StepStatus::Failed(String::new()),
        ];
        let glyphs: Vec<&str> = all.iter().map(step_glyph).collect();
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), glyphs.len(), "glyphs must be distinguishable");
    }
}
