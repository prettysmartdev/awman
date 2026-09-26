//! `ExecWorkflowCommandFrontend` impl for the TUI.

use crate::command::commands::exec_workflow::{
    ExecWorkflowCommandFrontend, WorkflowResumeDecision, WorkflowResumePrompt, WorkflowSummary,
};
use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;
use crate::data::workflow_state::PhaseKind;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl ExecWorkflowCommandFrontend for TuiCommandFrontend {
    fn report_workflow_context_path(&mut self, host_path: &std::path::Path) {
        if let Ok(mut path) = self.workflow_context_path.lock() {
            *path = Some(host_path.to_path_buf());
        }
    }

    /// A TUI always has a container window to show a phase step in.
    fn supports_interactive_phase_steps(&self) -> bool {
        true
    }

    /// Same slot preparation a sequential agent step gets in
    /// `report_step_interactive_launch`: fresh PTY channels, a parser reset so
    /// the previous container's screen is cleared, and no stale container name.
    /// The window is titled with the step, not the agent.
    fn report_phase_step_interactive_launch(&mut self, kind: PhaseKind) {
        self.pty_reset_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.recreate_container_io();
        if let Ok(mut name) = self.container_name_shared.lock() {
            *name = None;
        }
        let title = self
            .current_phase_step_title
            .clone()
            .unwrap_or_else(|| format!("[{}]", kind.label()));
        if let Ok(mut slot) = self.container_title_shared.lock() {
            *slot = Some(title);
        }
        self.messages.info(format!(
            "Launching {} step in new container...",
            kind.label()
        ));
    }

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        if let Ok(mut path) = self.workflow_context_path.lock() {
            *path = None;
        }
        self.messages.info(format!(
            "Workflow: {} completed, {} failed",
            summary.steps_completed, summary.steps_failed
        ));
        if summary.steps_failed > 0 {
            self.messages
                .error_msg(format!("Failed steps: {}", summary.steps_failed));
        }
    }

    fn ask_workflow_resume(
        &mut self,
        prompt: &WorkflowResumePrompt,
    ) -> Result<WorkflowResumeDecision, CommandError> {
        // Keys are '1'..'3' over the offered start points, in the order the
        // command layer built them (stopped / previous / next), plus 'f' to
        // start over.
        let mut keys: Vec<(char, String)> = prompt
            .choice_labels()
            .into_iter()
            .enumerate()
            .map(|(i, label)| (char::from_digit(i as u32 + 1, 10).unwrap_or('1'), label))
            .collect();
        keys.push(('f', prompt.fresh_label.clone()));

        let response = self.ask_dialog(DialogRequest::Custom {
            title: prompt.title.clone(),
            body: prompt.body.clone(),
            keys,
        })?;

        Ok(match response {
            DialogResponse::Char(c) => match c.to_digit(10) {
                Some(d) if d >= 1 => prompt
                    .start_points
                    .get(d as usize - 1)
                    .map(|p| WorkflowResumeDecision::ResumeFrom(p.name.clone()))
                    // A digit past the offered list: not an answer.
                    .unwrap_or(WorkflowResumeDecision::Cancel),
                // 'f' — the only way to discard the previous run.
                _ => WorkflowResumeDecision::Fresh,
            },
            // Esc cancels the command. Starting over deletes the previous run's
            // progress (and, in dynamic mode, its leader design), which is far
            // too destructive to be what dismissing a dialog means.
            _ => WorkflowResumeDecision::Cancel,
        })
    }

    fn notify_dynamic_workflow_resume_unavailable(
        &mut self,
        work_item: u32,
        reason: &str,
    ) -> Result<(), CommandError> {
        self.ask_dialog(DialogRequest::Custom {
            title: "Cannot resume previous workflow".into(),
            body: format!(
                "The worktree for work item {work_item:04} is still on disk, but the previous \
                 dynamic workflow cannot be resumed:\n\n{reason}\n\n\
                 A fresh dynamic workflow will be designed instead.",
            ),
            keys: vec![('c', "Continue — start a fresh dynamic workflow".into())],
        })?;
        Ok(())
    }
}
