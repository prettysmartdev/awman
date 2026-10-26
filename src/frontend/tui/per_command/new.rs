//! `NewCommandFrontend` impl for the TUI.

use crate::command::commands::new::{NewAgentOutcome, NewCommandFrontend};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl NewCommandFrontend for TuiCommandFrontend {
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
            default_text: None,
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

    /// Multi-line, through the same editor `new spec --interview` uses: this
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

    fn ask_skill_body(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: "Skill body".into(),
            prompt: "Enter the skill body content (Ctrl+Enter to submit):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::tui::per_command::mount_scope::tests::make_frontend;

    /// The skill summary is the entire brief the `--interview` agent works
    /// from, so it gets the multi-line editor the spec summary and the squad
    /// task description use — a one-line box could not hold one.
    #[test]
    fn the_skill_summary_step_opens_the_multiline_editor() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = std::thread::spawn(move || {
            let request = req_rx.recv().unwrap();
            resp_tx
                .send(DialogResponse::Text("line one\nline two".into()))
                .unwrap();
            request
        });

        let answer = frontend.ask_skill_summary().unwrap();
        let request = handle.join().unwrap();

        assert!(
            matches!(request, DialogRequest::MultilineInput { .. }),
            "the skill summary must open the multiline editor, got {request:?}"
        );
        assert_eq!(
            answer, "line one\nline two",
            "every line the user typed must reach the interview prompt"
        );
    }
}
