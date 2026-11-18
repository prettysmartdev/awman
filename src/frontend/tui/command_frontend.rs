//! The TUI's command frontend.
//!
//! The per-command frontend implementation that runs on a tab's command
//! thread is Layer 2 since WI 0142 (`command::instance::frontend::
//! InstanceCommandFrontend`, re-exported here as `TuiCommandFrontend`). What
//! stays in the TUI is [`TuiPresentation`]: the wording, glyph, box-drawing,
//! and host-output filtering decisions the producer asks its
//! presenter for, implemented with the same Layer 3 helpers as before.

pub use crate::command::instance::frontend::{
    DialogChannels, HostByteFilter, InstanceCommandFrontend as TuiCommandFrontend,
    InstancePresentation,
};

use crate::data::program_status::EffectiveStatus;
use crate::data::ready_summary::ReadySummary;
use crate::data::terminal::HostOutputPolicy;
use crate::frontend::terminal_output_filter::HostOutputFilter;

/// The TUI's [`InstancePresentation`]: exactly the helpers the command
/// frontend called directly before it moved to Layer 2.
#[derive(Debug, Clone, Copy, Default)]
pub struct TuiPresentation;

struct TuiHostFilter(HostOutputFilter);

impl HostByteFilter for TuiHostFilter {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.0.feed(chunk, out);
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        self.0.finish(out);
    }
}

impl InstancePresentation for TuiPresentation {
    fn ready_summary(&self, summary: &ReadySummary) -> String {
        let width = crossterm::terminal::size()
            .ok()
            .map(|(cols, _)| usize::from(cols))
            .filter(|width| *width > 0)
            .unwrap_or(80);
        let pane_width = crate::frontend::render_helpers::tui_ready_render_width(width);
        crate::frontend::render_helpers::render_ready_summary_with_width(summary, pane_width)
    }

    fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        crate::frontend::render_helpers::format_table(headers, rows)
    }

    fn status_short_label(&self, status: &EffectiveStatus) -> String {
        crate::frontend::program_status_text::short_label(status)
    }

    fn status_line(&self, status: &EffectiveStatus) -> String {
        crate::frontend::program_status_text::status_line(status)
    }

    fn host_output_filter(&self, policy: HostOutputPolicy) -> Box<dyn HostByteFilter> {
        Box::new(TuiHostFilter(HostOutputFilter::new(policy)))
    }
}

#[cfg(test)]
mod ready_failure_parity {
    use super::TuiPresentation;
    use crate::command::instance::feeds::TabCommandFeeds;
    use crate::command::instance::frontend::{DialogChannels, InstanceCommandFrontend};
    use crate::data::ready_summary::ReadySummary;
    use crate::data::step_status::StepStatus;
    use crate::engine::ready::frontend::ReadyFrontend;

    /// The CLI writes the shared renderer's box to stderr; the TUI puts the
    /// same box in its status log. Both must name the failed stage, the full
    /// cause and the recovery tip.
    #[test]
    fn cli_stderr_and_tui_status_log_convey_the_same_stage_cause_and_tip() {
        let cause = "no image source for awman-claude:1; set runtime.builtin.imageSource";
        let mut summary = ReadySummary::new("builtin");
        summary.agent_image = StepStatus::Failed(cause.into());
        let tip = summary.hint().expect("a failed image has a tip");

        let cli = crate::frontend::render_helpers::render_ready_summary_with_width(&summary, 200);

        let shared = TabCommandFeeds::new();
        let status_log = shared.status_log.clone();
        let (request_tx, _requests) = std::sync::mpsc::channel();
        let (_responses, response_rx) = std::sync::mpsc::channel();
        let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut frontend = InstanceCommandFrontend::new(
            crate::command::dispatch::parsed_input::parse(
                "ready",
                crate::command::dispatch::catalogue::CommandCatalogue::get(),
            )
            .unwrap(),
            DialogChannels::new(request_tx, response_rx),
            crate::engine::agent_runtime::AgentIo {
                stdout: stdout.clone(),
                stderr: stdout,
                stdin_tx,
                stdin_rx,
                resize: None,
                initial_size: None,
            },
            shared,
        )
        .with_presentation(std::sync::Arc::new(TuiPresentation));
        frontend.report_summary(&summary);
        let tui = status_log
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");

        // Ignore layout: whitespace, box drawing and wrapping.
        let flat = |s: &str| -> String { s.chars().filter(|c| c.is_ascii_graphic()).collect() };
        let rows = summary.rows();
        let (stage, _) = rows
            .iter()
            .find(|(_, s)| matches!(s, StepStatus::Failed(_)))
            .expect("a failed row");
        for (name, shown) in [("cli", &cli), ("tui", &tui)] {
            let shown = flat(shown);
            assert!(shown.contains(&flat(stage)), "{name}: stage {stage}");
            assert!(shown.contains(&flat(cause)), "{name}: cause");
            assert!(shown.contains(&flat(&tip)), "{name}: tip {tip}");
        }
    }
}
