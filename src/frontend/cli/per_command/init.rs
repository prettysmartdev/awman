//! `InitFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) for aspec replacement,
//! audit, and work-items config; otherwise it returns the safe
//! non-interactive defaults.

use crate::data::config::repo::WorkItemsConfig;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::{InitFrontend, InitPhase, InitSummary};
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

use super::helpers::{render_summary_box, step_status_label, yes_no};

impl InitFrontend for CliFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        eprintln!();
        eprintln!("amux: The aspec/ folder contains your project specification files —");
        eprintln!("amux: architecture docs, design decisions, and work item templates.");
        eprintln!("amux: Replacing it will overwrite any customisations you've made.");
        eprintln!();
        Ok(yes_no(
            "An aspec/ folder already exists. Replace it with fresh templates?",
            false,
        ))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        eprintln!();
        eprintln!("amux: The agent audit scans your repository and tailors the");
        eprintln!("amux: Dockerfile.dev for your project's language, build tools,");
        eprintln!("amux: and dependencies. It runs inside a container and does not");
        eprintln!("amux: modify your repository — only the generated Dockerfile.");
        eprintln!();
        Ok(yes_no(
            "Run the agent audit container to scan and customise the Dockerfile?",
            false,
        ))
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        if !stdin_is_tty() {
            return Ok(None);
        }
        eprintln!(
            "amux: Configure a work items directory? (path relative to repo root, empty to skip)"
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(None);
        }
        let dir = buf.trim();
        if dir.is_empty() {
            return Ok(None);
        }
        eprintln!("amux: Work item template path (empty for none):");
        let mut buf2 = String::new();
        let _ = std::io::stdin().read_line(&mut buf2);
        let template_str = buf2.trim();
        let template = if template_str.is_empty() {
            None
        } else {
            Some(template_str.to_string())
        };
        Ok(Some(WorkItemsConfig {
            dir: Some(dir.to_string()),
            template,
        }))
    }

    fn report_phase(&mut self, _phase: &InitPhase) {
        // InitPhase is an internal state-machine token; users see progress
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

    fn report_summary(&mut self, summary: &InitSummary) {
        let rows: Vec<(&str, &StepStatus)> = vec![
            ("Config", &summary.config),
            ("aspec folder", &summary.aspec_folder),
            ("Dockerfile.dev", &summary.dockerfile),
            ("Agent audit", &summary.audit),
            ("Docker image", &summary.image_build),
            ("Work items", &summary.work_items_setup),
        ];
        let box_str = render_summary_box("Init Summary", &rows);
        let footer = "\nWhat's Next?\n  Run `amux` to launch the interactive TUI.\n\n  Available commands:\n    amux chat          — Start a freeform chat session with the agent\n    amux new spec      — Create a new work item from the aspec template\n    amux exec workflow — Run a workflow inside a container\n";
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("\n{box_str}{footer}").as_bytes(),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}
