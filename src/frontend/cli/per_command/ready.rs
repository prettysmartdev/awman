//! `ReadyFrontend` impl for the CLI.
//!
//! Per WI 0069 §1, prompts on stdin for Dockerfile and legacy-migration
//! decisions when stdin is a TTY; otherwise returns the safe defaults
//! from §7u.

use crate::data::session::AgentName;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::ready::{ReadyFrontend, ReadyPhase, ReadySummary};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::{render_summary_box, step_status_label, yes_no};

impl ReadyFrontend for CliFrontend {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no(
            "No Dockerfile.dev found in the project. Create one from the default template?",
            true,
        ))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no(
            "Run the agent audit container to scan and customise the Dockerfile?",
            false,
        ))
    }

    fn ask_migrate_legacy_layout(&mut self, agent_name: &AgentName) -> Result<bool, EngineError> {
        Ok(yes_no(
            &format!(
                "Legacy single-Dockerfile layout detected. Migrate to the modular layout for agent '{}'?",
                agent_name.as_str()
            ),
            false,
        ))
    }

    fn report_phase(&mut self, _phase: &ReadyPhase) {
        // The ReadyPhase enum is an internal state-machine token; users see
        // progress through `report_step_status` and the final summary box.
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

    fn report_summary(&mut self, summary: &ReadySummary) {
        let rows: Vec<(&str, &StepStatus)> = vec![
            ("Base image", &summary.base_image),
            ("Agent image", &summary.agent_image),
            ("Local agent", &summary.local_agent),
            ("Audit", &summary.audit),
            ("Legacy migration", &summary.legacy_migration),
        ];
        let box_str = render_summary_box(
            &format!("Ready Summary ({})", summary.runtime_name),
            &rows,
        );
        // Write the summary box directly to stderr without the per-line
        // "amux:" prefix used for status updates — the box is multi-line
        // content that reads better unprefixed.
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n{box_str}amux is ready.\n").as_bytes(),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}
