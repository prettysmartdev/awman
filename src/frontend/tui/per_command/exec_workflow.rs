//! `ExecWorkflowCommandFrontend` impl for the TUI.

use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl ExecWorkflowCommandFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.messages.info(format!(
            "Workflow: {} completed, {} failed",
            summary.steps_completed, summary.steps_failed
        ));
        if summary.steps_failed > 0 {
            self.messages
                .error_msg(format!("Failed steps: {}", summary.steps_failed));
        }
    }

    fn ask_workflow_resume_or_fresh(
        &mut self,
        workflow_name: &str,
        completed_steps: usize,
        total_steps: usize,
    ) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Existing workflow state".into(),
            body: format!(
                "Persisted state found for workflow '{}'.\n\n\
                 Progress: {}/{} step(s) completed.\n\n\
                 Resume from where it left off, or delete the state and start fresh?",
                workflow_name, completed_steps, total_steps,
            ),
            keys: vec![
                ('r', "Resume from saved state".into()),
                ('f', "Delete state and start fresh".into()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('f') | DialogResponse::Char('F') => false,
            // Default to resume on dismiss (Esc) — safer than wiping state.
            _ => true,
        })
    }
}
