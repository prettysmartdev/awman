//! `NewCommandFrontend` impl for the TUI.

use crate::command::commands::new::{NewAgentOutcome, NewCommandFrontend};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse};

impl NewCommandFrontend for InstanceCommandFrontend {
    fn ask_agent_summary(&mut self) -> Result<String, CommandError> {
        match self.ask_dialog(DialogRequest::MultilineInput {
            title: "Agent summary".into(),
            prompt: "Describe the agent definition to create:".into(),
            default_text: None,
        })? {
            DialogResponse::Text(text) => Ok(text),
            _ => Err(CommandError::Aborted),
        }
    }
    /// One status-log entry per line: the log draws each entry as one line.
    fn show_new_agent(&mut self, outcome: &NewAgentOutcome) {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        for line in outcome.render().lines() {
            self.write_message(UserMessage {
                level: MessageLevel::Info,
                text: line.to_string(),
            });
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

    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Skill name".into(),
            prompt: "Enter the skill name:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.is_empty() => Ok(t),
            _ => Err(CommandError::Aborted),
        }
    }

    /// Multi-line, through the same editor `new spec` uses: this
    /// text is the whole brief the interview agent works from, and a one-line
    /// box cannot hold one.
    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: "Skill summary".into(),
            prompt: "Describe what the skill should do (Ctrl+Enter to submit):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }
}
