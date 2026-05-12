//! `SpecsCommandFrontend` impl for the TUI.

use crate::command::commands::specs::{SpecsCommandFrontend, WorkItemKind};
use crate::command::error::CommandError;
use crate::engine::container::frontend::ContainerFrontend;
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
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Ok(String::new()),
        }
    }

    fn ask_spec_kind(&mut self) -> Result<WorkItemKind, CommandError> {
        let response = self.ask_dialog(DialogRequest::KindSelect {
            title: "Work item kind".into(),
            options: vec![
                ("1".into(), "Feature".into()),
                ("2".into(), "Bug".into()),
                ("3".into(), "Task".into()),
                ("4".into(), "Enhancement".into()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('1') | DialogResponse::Index(0) => WorkItemKind::Feature,
            DialogResponse::Char('2') | DialogResponse::Index(1) => WorkItemKind::Bug,
            DialogResponse::Char('3') | DialogResponse::Index(2) => WorkItemKind::Task,
            DialogResponse::Char('4') | DialogResponse::Index(3) => WorkItemKind::Enhancement,
            _ => WorkItemKind::Task,
        })
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }

    fn container_frontend_for_pty(&mut self) -> Box<dyn ContainerFrontend> {
        match self.container_io.take() {
            Some(io) => Box::new(super::TuiContainerProxy::with_io(
                self.status_log.clone(),
                io,
                self.container_name_shared.clone(),
            )),
            None => Box::new(super::TuiContainerProxy::new(self.status_log.clone())),
        }
    }

    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }
}
