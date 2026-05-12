//! `AgentSetupFrontend` and `HasContainerFrontend` impls for the TUI.

use crate::command::commands::agent_setup::{
    AgentSetupDecision, AgentSetupFrontend, HasContainerFrontend,
};
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{AgentSetupState, DialogRequest, DialogResponse};

impl AgentSetupFrontend for TuiCommandFrontend {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        let has_fallback = default_available && default.as_str() != requested.as_str();
        let response = self.ask_dialog(DialogRequest::AgentSetup(AgentSetupState {
            agent_name: requested.as_str().to_string(),
            image_only,
            has_fallback,
            fallback_name: if has_fallback {
                Some(default.as_str().to_string())
            } else {
                None
            },
        }))?;
        Ok(match response {
            DialogResponse::Char('y') | DialogResponse::Yes => AgentSetupDecision::Setup,
            DialogResponse::Char('f') if default_available => AgentSetupDecision::FallbackToDefault,
            _ => AgentSetupDecision::Abort,
        })
    }

    fn record_fallback(&mut self, _requested: &AgentName, fallback: &AgentName) {
        self.messages
            .info(format!("Falling back to agent {}", fallback.as_str()));
    }
}

impl HasContainerFrontend for TuiCommandFrontend {
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }

    fn container_frontend_for_pty(&mut self) -> Box<dyn ContainerFrontend> {
        // Hand the PTY-bridge channels to the engine so the container's PTY
        // master is wired directly to the TUI's vt100 parser. After this the
        // engine drives all stdout/stdin/resize traffic; the TuiCommandFrontend
        // continues to be used for status messages and dialog prompts.
        match self.container_io.take() {
            Some(io) => Box::new(super::TuiContainerProxy::with_io(
                self.status_log.clone(),
                io,
                self.container_name_shared.clone(),
            )),
            None => Box::new(super::TuiContainerProxy::new(self.status_log.clone())),
        }
    }
}
