//! `NewCommandFrontend` impl for the TUI.

use crate::command::commands::new::NewCommandFrontend;
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl NewCommandFrontend for TuiCommandFrontend {
    fn ask_workflow_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Workflow name".into(),
            prompt: "Enter the workflow filename slug:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.is_empty() => Ok(t),
            _ => Ok("workflow".to_string()),
        }
    }

    fn ask_workflow_title(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Workflow title".into(),
            prompt: "Enter a human-readable workflow title:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }

    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Workflow summary".into(),
            prompt: "Enter a one-line summary:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }

    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Step name".into(),
            prompt: "Enter the step name:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_workflow_step_agent(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Step agent".into(),
            prompt: "Agent override (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.is_empty() => Ok(Some(t)),
            _ => Ok(None),
        }
    }

    fn ask_workflow_step_model(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Step model".into(),
            prompt: "Model override (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.is_empty() => Ok(Some(t)),
            _ => Ok(None),
        }
    }

    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: "Step prompt".into(),
            prompt: "Enter the step prompt (Ctrl+Enter to submit):".into(),
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }

    fn ask_add_another_step(&mut self) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Add another step?".into(),
            body: "Would you like to add another step to this workflow?".into(),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            _ => Ok(false),
        }
    }

    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Skill name".into(),
            prompt: "Enter the skill name:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.is_empty() => Ok(t),
            _ => Ok("skill".to_string()),
        }
    }

    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Skill summary".into(),
            prompt: "Enter a one-line skill summary:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }

    fn ask_skill_body(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: "Skill body".into(),
            prompt: "Enter the skill body content (Ctrl+Enter to submit):".into(),
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }
}
