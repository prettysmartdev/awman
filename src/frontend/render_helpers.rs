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
    let values: Vec<_> = rows
        .iter()
        .map(|(label, status)| (*label, format!("{} {}", step_glyph(status), status.label())))
        .collect();
    render_summary_values(title, &values)
}

fn render_summary_values(title: &str, rows: &[(&str, String)]) -> String {
    let label_w = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(8)
        .max(16);
    let value_w = rows
        .iter()
        .map(|(_, value)| value.chars().count())
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
    for (label, value) in rows {
        let label_pad = label_w.saturating_sub(label.chars().count());
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
    let values: Vec<_> = rows
        .iter()
        .map(|(label, status)| {
            let value = if label.starts_with("Auth: ") {
                match status {
                    StepStatus::Done => "success".to_string(),
                    StepStatus::Failed(_) => "fail".to_string(),
                    _ => status.label(),
                }
            } else {
                status.label()
            };
            (label.as_str(), format!("{} {value}", step_glyph(status)))
        })
        .collect();
    render_summary_values(
        &format!("Ready Summary ({})", summary.runtime_name),
        &values,
    )
}

/// A box-drawn table sized by terminal display width, shared by CLI and TUI.
pub(crate) fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    use unicode_width::UnicodeWidthStr as _;
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.width()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.width());
        }
    }
    let mut out = String::new();
    out.push('┌');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┬' } else { '┐' });
    }
    out.push('\n');
    out.push('│');
    for (h, w) in headers.iter().zip(widths.iter()) {
        let pad = w.saturating_sub(h.width());
        out.push_str(&format!(" {h}{} │", " ".repeat(pad)));
    }
    out.push('\n');
    out.push('├');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┼' } else { '┤' });
    }
    out.push('\n');
    for row in rows {
        out.push('│');
        for (cell, w) in row.iter().zip(widths.iter()) {
            let pad = w.saturating_sub(cell.width());
            out.push_str(&format!(" {cell}{} │", " ".repeat(pad)));
        }
        out.push('\n');
    }
    out.push('└');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┴' } else { '┘' });
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod ready_auth_tests {
    use super::*;
    use crate::command::commands::ready::ReadyOutcome;
    use crate::data::ready_summary::ReadySummary;

    #[test]
    fn auth_table_groups_agents_and_outcome_preserves_failure_details() {
        let mut summary = ReadySummary::new("docker");
        summary.agent_auth = vec![
            ("claude".into(), StepStatus::Done),
            ("codex".into(), StepStatus::Done),
        ];
        let table = render_ready_summary(&summary);
        let rows: Vec<_> = table
            .lines()
            .filter(|line| line.contains("Auth:"))
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("Auth: claude, codex"));
        assert!(rows[0].contains("success"));
        assert!(!table.contains("Local agent"));

        summary.agent_auth.push((
            "opencode".into(),
            StepStatus::Failed("not installed".into()),
        ));
        summary.local_agent = StepStatus::Failed("opencode: not installed".into());
        let table = render_ready_summary(&summary);
        let rows: Vec<_> = table
            .lines()
            .filter(|line| line.contains("Auth:"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("Auth: claude, codex") && rows[0].contains("success"));
        assert!(rows[1].contains("Auth: opencode") && rows[1].contains("fail"));
        let outcome = ReadyOutcome::from(summary.clone());
        assert_eq!(outcome.agent_auth, summary.agent_auth);
        let json = outcome.to_legacy_json();
        assert_eq!(json["agent_auth"].as_array().unwrap().len(), 3);
        assert_eq!(json["steps"]["local_agent"]["status"], "failed");
        assert_eq!(json["agent_auth"][2][1]["Failed"], "not installed");

        // If every attempted greeting fails, no empty success row is shown.
        summary.agent_auth = vec![(
            "opencode".into(),
            StepStatus::Failed("not installed".into()),
        )];
        let table = render_ready_summary(&summary);
        let rows: Vec<_> = table
            .lines()
            .filter(|line| line.contains("Auth:"))
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("Auth: opencode") && rows[0].contains("fail"));
    }
}
