//! `ClawsFrontend` impl for the CLI.

use std::path::Path;

use crate::engine::claws::{ClawsFrontend, ClawsPhase, ClawsSummary};
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::{render_summary_box, step_status_label, yes_no};

impl ClawsFrontend for CliFrontend {
    fn ask_replace_existing_clone(&mut self, path: &Path) -> Result<bool, EngineError> {
        Ok(yes_no(
            &format!(
                "An existing nanoclaw clone was found at {}. Replace it?",
                path.display()
            ),
            false,
        ))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no(
            "Run the nanoclaw audit container now?",
            false,
        ))
    }

    fn report_phase(&mut self, _phase: &ClawsPhase) {
        // ClawsPhase is an internal state-machine token; users see progress
        // through `report_step_status` and the final summary box.
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        let level = match status {
            StepStatus::Failed(_) => MessageLevel::Error,
            _ => MessageLevel::Info,
        };
        self.messages.write_message(UserMessage {
            level,
            text: format!("{step}: {}", step_status_label(&status)),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn report_summary(&mut self, summary: &ClawsSummary) {
        let rows: Vec<(&str, &StepStatus)> = vec![
            ("Clone", &summary.clone),
            ("Permissions", &summary.permissions_check),
            ("Image build", &summary.image_build),
            ("Audit", &summary.audit),
            ("Configure", &summary.configure),
            ("Controller", &summary.controller),
        ];
        let box_str = render_summary_box("Claws Summary", &rows);
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n{box_str}").as_bytes(),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}
