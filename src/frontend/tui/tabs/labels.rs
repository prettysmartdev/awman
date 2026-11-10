//! Tab-bar label formatting: project name, subcommand label (including the
//! background yolo-countdown variant), and the workflow step suffix.

use super::*;
use crate::data::program_status::{EffectiveStatus, ProgramState};
use crate::frontend::program_status_text::tab_glyph;

impl Tab {
    /// Project name for the tab title, truncated to fit a `tab_width`-wide
    /// tab cell. The rendered title is `" ➡ {name} "` inside the two border
    /// cells — 6 chars of chrome in the active variant, which sizing always
    /// reserves — so the name may use `tab_width - 6` chars. Wide tabs show
    /// more of a long project name instead of clipping at a fixed length;
    /// pass `u16::MAX` to measure the untruncated name.
    pub fn project_name(&self, tab_width: u16) -> String {
        if let Some(label) = &self.label_override {
            let max = (tab_width as usize).saturating_sub(6).max(1);
            return truncate_with_ellipsis(label, max);
        }
        let name = if self.is_squad {
            // Fixed label: `AWMAN_SQUAD_ROOT` may point at a directory whose
            // basename has nothing to do with squad.
            "squad".to_string()
        } else {
            self.session
                .working_dir()
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string()
        };
        let max = (tab_width as usize).saturating_sub(6).max(1);
        truncate_with_ellipsis(&name, max)
    }

    /// Yolo countdown label for background tabs: alternates emoji + countdown.
    /// Returns `None` when no yolo countdown is active.
    pub fn background_yolo_label(&self, tab_width: u16) -> Option<String> {
        let state = self.shared.yolo_state.lock().ok()?.as_ref()?.clone();
        let label = if state.remaining_secs % 2 == 0 {
            format!("\u{26a0}\u{fe0f}  yolo in {}", state.remaining_secs)
        } else {
            format!("\u{1f918} yolo in {}", state.remaining_secs)
        };
        let max_chars = tab_width.saturating_sub(4) as usize;
        let truncated = if label.chars().count() > max_chars && max_chars > 1 {
            let t: String = label.chars().take(max_chars - 1).collect();
            format!("{}\u{2026}", t)
        } else {
            label
        };
        Some(truncated)
    }

    /// Subcommand label rendered inside the tab cell (NOT in the title).
    /// Empty while Idle. Prepended with `⚠️ ` while stuck. Truncated to fit
    /// `tab_width - 4` chars (2 borders + 2 padding spaces).
    /// For background tabs with an active yolo countdown, shows the countdown
    /// label instead.
    pub fn tab_subcommand_label(&self, tab_width: u16, is_active: bool) -> String {
        if !is_active {
            if let Some(label) = self.background_yolo_label(tab_width) {
                return label;
            }
        }
        let cmd = match &self.execution_phase {
            ExecutionPhase::Idle => return String::new(),
            ExecutionPhase::Running { command }
            | ExecutionPhase::Done { command, .. }
            | ExecutionPhase::Error { command, .. } => command.as_str(),
        };

        // When a workflow is active, append step info: "exec workflow: step (N/M)"
        let workflow_suffix = self.workflow_step_suffix();
        let display = if workflow_suffix.is_empty() {
            cmd.to_string()
        } else {
            format!("{}: {}", cmd, workflow_suffix)
        };

        // A reporting agent shows its reported-state glyph; only agents that do
        // not report can be flagged stuck (silence).
        let prefix = match self.program_status() {
            Some(status) => match tab_glyph(&status) {
                glyph if glyph.is_empty() => String::new(),
                glyph => format!("{glyph} "),
            },
            None if self.stuck => "\u{26a0}\u{fe0f} ".to_string(),
            None => String::new(),
        };
        let prefix_chars = prefix.chars().count();
        let max_chars = (tab_width as usize).saturating_sub(4);
        let cmd_max = max_chars.saturating_sub(prefix_chars);
        let cmd_str = if display.chars().count() > cmd_max && cmd_max > 1 {
            let truncated: String = display.chars().take(cmd_max - 1).collect();
            format!("{}\u{2026}", truncated)
        } else {
            display
        };
        format!("{}{}", prefix, cmd_str)
    }

    /// Build a workflow step suffix like "implement (2/5)" for the tab label.
    /// Returns empty string when no workflow is active or has no steps.
    fn workflow_step_suffix(&self) -> String {
        let guard = self.shared.workflow.snapshot().view;
        let view = match guard.as_ref() {
            Some(v) if !v.steps.is_empty() => v,
            _ => return String::new(),
        };
        let total = view.steps.len();
        let done_count = view
            .steps
            .iter()
            .filter(|s| s.status == StepViewStatus::Done)
            .count();
        let current_name = view.current_step.as_deref().unwrap_or_else(|| {
            view.steps
                .iter()
                .find(|s| s.status == StepViewStatus::Running)
                .map(|s| s.name.as_str())
                .unwrap_or("")
        });
        if current_name.is_empty() {
            // Workflow finished or not yet started
            let completed = done_count == total;
            if completed {
                return format!("done ({}/{})", total, total);
            }
            return String::new();
        }
        let step_index = view
            .steps
            .iter()
            .position(|s| s.name == current_name)
            .map(|i| i + 1)
            .unwrap_or(0);
        format!("{} ({}/{})", current_name, step_index, total)
    }

    pub fn subcommand_label(&self) -> &str {
        match &self.execution_phase {
            ExecutionPhase::Idle => "",
            ExecutionPhase::Running { command } => command.as_str(),
            ExecutionPhase::Done { command, .. } => command.as_str(),
            ExecutionPhase::Error { command, .. } => command.as_str(),
        }
    }
}

/// How long a reporting agent must be silent before the UI notes it
/// ("working · silent 12m"). Information only: no action is ever taken.
const SILENT_NOTE_AFTER_SECS: u64 = 60;

impl ContainerSlot {
    /// Store the agent's latest reported status. A reporting agent is never
    /// stuck: the silence flag is cleared as soon as a report arrives.
    pub fn set_program_status(&mut self, status: Option<EffectiveStatus>) {
        if status.is_some() {
            self.stuck = false;
        }
        self.program_status = status;
    }

    /// `working · silent 12m` for a working agent that has produced no output
    /// for a while; `None` otherwise.
    pub fn silent_note(&self) -> Option<String> {
        let status = self.program_status.as_ref()?;
        if status.state != ProgramState::Working {
            return None;
        }
        let silent = self.last_output_at.elapsed().as_secs();
        (silent >= SILENT_NOTE_AFTER_SECS)
            .then(|| format!("working \u{b7} silent {}", format_duration(silent)))
    }
}

impl Tab {
    /// The tab's aggregate reported status across its container slots.
    pub fn program_status(&self) -> Option<EffectiveStatus> {
        EffectiveStatus::most_urgent(
            self.container_slots
                .iter()
                .filter_map(|s| s.program_status.as_ref()),
        )
        .cloned()
    }
}
