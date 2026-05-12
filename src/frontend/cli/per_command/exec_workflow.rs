//! `ExecWorkflowCommandFrontend` impl for the CLI.
//!
//! All supertraits (`UserMessageSink`, `ContainerFrontend`, `WorkflowFrontend`,
//! `MountScopeFrontend`, `AgentSetupFrontend`, `AgentAuthFrontend`,
//! `WorktreeLifecycleFrontend`) are implemented elsewhere in
//! `src/frontend/cli/`; this file only carries the trait's two extra methods
//! (`set_pty_active` and `report_workflow_summary`).

use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::error::CommandError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

use crate::frontend::cli::command_frontend::CliFrontend;

impl ExecWorkflowCommandFrontend for CliFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.messages.set_pty_active(active);
    }

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "workflow summary — {}/{} steps OK ({} failed)",
                summary.steps_completed,
                summary.steps_completed + summary.steps_failed,
                summary.steps_failed
            ),
        });
    }

    fn ask_workflow_resume_or_fresh(
        &mut self,
        _workflow_name: &str,
        _completed_steps: usize,
        _total_steps: usize,
    ) -> Result<bool, CommandError> {
        // Non-interactive default: resume from saved state. The CLI/headless
        // path has no dialog system; preserving state is the safer choice
        // (matches old-amux behavior).
        Ok(true)
    }
}
