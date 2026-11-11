//! `ReadyFrontend` impl for the TUI.

use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::UserMessageSink;
use crate::data::ready_phase::ReadyPhase;
use crate::data::ready_summary::ReadySummary;
use crate::engine::error::EngineError;
use crate::engine::ready::frontend::ReadyFrontend;

impl ReadyFrontend for InstanceCommandFrontend {
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.create_dockerfile());
        }
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Create Dockerfile?".into(),
                body: format!(
                    "No Dockerfile found at {}. Create one from the default template?",
                    dockerfile_path.display()
                ),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.run_audit_on_template());
        }
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Run audit?".into(),
                body: "Dockerfile.dev matches the default template. Run the audit to install project dependencies?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        // The shared `ReadyPhase` Display text, as the API reports it.
        self.messages.info(format!("ready: {phase}"));
    }

    fn report_summary(&mut self, summary: &ReadySummary) {
        // The rows, outcome line and hint are the summary's own (F-23;
        // readiness now accounts for per-image build/import results too):
        // this renders them without recomputing any of that itself.
        let text = self.presentation.ready_summary(summary);
        for line in text.lines() {
            self.messages.info(line.to_string());
        }

        if let Some(hint) = summary.hint() {
            self.messages.info(format!("Tip: {hint}"));
        }

        if summary.is_ready() {
            self.messages.success(summary.outcome_line().to_string());
        } else {
            self.messages.error_msg(summary.outcome_line().to_string());
        }
    }
}
