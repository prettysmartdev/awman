//! `SpecsCommandFrontend` impl for the TUI.

use crate::command::commands::specs::{SpecsCommandFrontend, WorkItemKind};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl SpecsCommandFrontend for TuiCommandFrontend {
    fn ask_spec_title(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Spec title".into(),
            prompt: "Enter the work item title:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.is_empty() => Ok(t),
            _ => Ok("Untitled work item".to_string()),
        }
    }

    fn ask_spec_summary(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: "Spec summary".into(),
            prompt: "Enter a brief summary (Ctrl+Enter to submit):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }

    /// Renders `prompt`'s own choices and maps the answer back through it —
    /// the labels and hotkeys are Layer 2's (F-19).
    fn ask_spec_kind(
        &mut self,
        prompt: &crate::data::prompt::Prompt<WorkItemKind>,
    ) -> Result<WorkItemKind, CommandError> {
        // No dismissal default on this prompt: abandoning the interview
        // abandons it, which is what `pick_from_prompt` does with a `None`.
        self.pick_from_prompt(prompt)
    }
}
