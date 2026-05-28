use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Skipped,
    Running,
    Done,
    Warn(String),
    Failed(String),
}

impl StepStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StepStatus::Skipped | StepStatus::Done | StepStatus::Warn(_) | StepStatus::Failed(_)
        )
    }

    pub fn label(&self) -> String {
        match self {
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

    pub fn glyph(&self) -> &'static str {
        match self {
            StepStatus::Pending => "-",
            StepStatus::Running => "\u{2026}",
            StepStatus::Done => "\u{2713}",
            StepStatus::Skipped => "\u{2013}",
            StepStatus::Warn(_) => "\u{26a0}",
            StepStatus::Failed(_) => "\u{2717}",
        }
    }
}

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
        let value = format!("{} {}", status.glyph(), status.label());
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
